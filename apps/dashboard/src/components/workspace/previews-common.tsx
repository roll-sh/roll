import type { MarkState } from "@roll/brand";
import { useCallback, useEffect, useRef, useState } from "react";
import { useMarkState } from "@/components/mark-state";
import { BUSY, previewsApi, type Preview, type PreviewState } from "@/lib/previews-api";
import { formatUsd } from "@/lib/roll-api";

/** How often the list is read: quickly while a preview is changing, calmly otherwise. */
export const POLL_BUSY_MS = 3_000;
export const POLL_CALM_MS = 20_000;

export const STATE_LABEL: Record<PreviewState, string> = {
  queued: "Queued",
  awaiting_approval: "Awaiting approval",
  creating: "Creating",
  ready: "Ready",
  sleeping: "Asleep",
  updating: "Updating",
  failed: "Failed",
  deleting: "Deleting",
};

const DOT: Record<string, string> = {
  ready: "bg-ok",
  running: "bg-ok",
  sleeping: "bg-subtle",
  queued: "shadow-[inset_0_0_0_1.25px_var(--subtle)]",
  pending: "shadow-[inset_0_0_0_1.25px_var(--subtle)]",
  awaiting_approval: "bg-accent",
  creating: "pulse-dot bg-accent",
  updating: "pulse-dot bg-accent",
  deploying: "pulse-dot bg-accent",
  resetting: "pulse-dot bg-accent",
  deleting: "pulse-dot bg-muted",
  failed: "bg-danger",
};

/** A preview's, service's or branch's state as a dot (busy ones pulse calmly). */
export function StateDot({ state, className = "" }: { state: string; className?: string }) {
  return <i aria-hidden className={`inline-block size-1.5 shrink-0 rounded-full ${DOT[state] ?? "bg-subtle"} ${className}`} />;
}

/** `#482`, `PR #482` or the branch: what the preview is of. */
export function previewOf(p: Pick<Preview, "github_pr" | "number" | "branch">) {
  if (p.github_pr != null) return `PR #${p.github_pr}`;
  if (p.number != null) return `#${p.number}`;
  return p.branch ?? "";
}

export const host = (url: string) => url.replace(/^https?:\/\//, "").replace(/\/$/, "");

/** A time in unix milliseconds, as "3m ago". */
export function since(ms: number, now = Date.now()) {
  const s = Math.max(0, Math.floor((now - ms) / 1000));
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

export const money = formatUsd;

export const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/**
 * A project's previews, newest first, read every few seconds while one is creating, updating or
 * deleting and calmly otherwise. `put` takes a preview an action answered with, and reads again
 * soon if it's busy.
 */
export function usePreviews(projectId: string) {
  const [list, setList] = useState<Preview[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const live = useRef(true);

  const load = useCallback(async () => {
    clearTimeout(timer.current);
    let busy = false;
    try {
      const l = await previewsApi.list({ project: projectId });
      if (!live.current) return;
      setList(l);
      setError(null);
      busy = l.some((p) => BUSY.has(p.state));
    } catch (e) {
      if (!live.current) return;
      setError(errorText(e));
    }
    clearTimeout(timer.current);
    timer.current = setTimeout(load, busy ? POLL_BUSY_MS : POLL_CALM_MS);
  }, [projectId]);

  useEffect(() => {
    live.current = true;
    void load();
    return () => {
      live.current = false;
      clearTimeout(timer.current);
    };
  }, [load]);

  const put = useCallback(
    (p: Preview) => {
      setList((l) => (l ? (l.some((x) => x.id === p.id) ? l.map((x) => (x.id === p.id ? p : x)) : [p, ...l]) : [p]));
      if (BUSY.has(p.state)) {
        clearTimeout(timer.current);
        timer.current = setTimeout(load, POLL_BUSY_MS);
      }
    },
    [load],
  );

  return { list, error, reload: load, put };
}

/** The mark state a set of previews holds: deploying while one changes, asleep when all sleep. */
function steadyOf(list: Preview[]): MarkState {
  if (list.some((p) => p.state === "creating" || p.state === "updating")) return "deploying";
  if (list.length > 0 && list.every((p) => p.state === "sleeping")) return "sleep";
  return "idle";
}

/**
 * Plays the previews on every mark (AGENTS.md "Motion"): `deploying` while one is creating or
 * updating, `success` when one becomes ready, `error` when one fails, `sleep` while the ones in
 * view are asleep. `act` wraps an action the user started the same way. Back to idle on unmount.
 */
export function usePreviewMark(inView: Preview[] | null) {
  const { setState, play } = useMarkState();
  const was = useRef<Map<string, PreviewState> | null>(null);
  const steady = useRef<MarkState | null>(null);

  useEffect(() => {
    if (!inView) return;
    const before = was.current;
    was.current = new Map(inView.map((p) => [p.id, p.state]));
    const next = steadyOf(inView);
    if (before) {
      const turned = (to: PreviewState, from?: (s: PreviewState) => boolean) =>
        inView.some((p) => p.state === to && before.has(p.id) && before.get(p.id) !== to && (!from || from(before.get(p.id)!)));
      if (turned("failed")) {
        steady.current = next;
        return play([["error", 1500]], next);
      }
      if (turned("ready", (s) => s === "creating" || s === "updating")) {
        steady.current = next;
        return play([["success", 1400]], next);
      }
    }
    if (steady.current !== next) {
      steady.current = next;
      setState(next);
    }
  }, [inView, play, setState]);

  useEffect(() => () => setState("idle"), [setState]);

  return useCallback(
    async <T,>(fn: () => Promise<T>): Promise<T> => {
      setState("deploying");
      try {
        const r = await fn();
        play([["success", 1400]], steady.current ?? "idle");
        return r;
      } catch (e) {
        play([["error", 1500]], steady.current ?? "idle");
        throw e;
      }
    },
    [play, setState],
  );
}
