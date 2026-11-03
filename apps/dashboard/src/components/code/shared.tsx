"use client";

import {
  CircleCheck,
  CircleDashed,
  CircleX,
  FileClock,
  GitMerge,
  GitMergeConflict,
  GitPullRequest,
  GitPullRequestClosed,
  ListOrdered,
  LoaderCircle,
  Lock,
  ShieldQuestion,
  type LucideIcon,
} from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";
import { mayPulse, prependBatch, prune, windowRate } from "@/lib/live-rate";
import {
  subscribe,
  type Blocker,
  type ChangeState,
  type Check,
  type CheckStatus,
  type LandedEvent,
  type QueueEvent,
  type QueueInfo,
  type Visibility,
} from "@/lib/code-api";
import { blockerCopy, shouldPlayLanding, type BlockerAction } from "@/lib/queue";
import { useMarkState } from "../mark-state";

export { POLL_MS, Field, Segmented, useAction, type Run } from "../redis/shared";

/** `public` / `private` (with a lock), next to repo names. */
export function VisibilityBadge({ visibility }: { visibility: Visibility }) {
  return (
    <span className="inline-flex items-center gap-1 rounded-full border border-line px-2 py-0.5 text-[11px] font-normal tracking-normal text-muted">
      {visibility === "private" && <Lock className="size-3" />}
      {visibility}
    </span>
  );
}

/** The GitHub mark (lucide has no brand icons), drawn with currentColor at icon size. */
export function GitHubMark({ className = "size-3.5" }: { className?: string }) {
  return (
    <svg viewBox="0 0 16 16" aria-hidden className={className} fill="currentColor">
      <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z" />
    </svg>
  );
}

const changeStyle: Record<ChangeState, { icon: typeof GitMerge; cls: string; label: string }> = {
  open: { icon: GitPullRequest, cls: "text-fg", label: "open" },
  queued: { icon: GitPullRequest, cls: "text-accent", label: "queued" },
  landed: { icon: GitMerge, cls: "text-ok", label: "landed" },
  conflict: { icon: GitMergeConflict, cls: "text-danger", label: "conflict" },
  closed: { icon: GitPullRequestClosed, cls: "text-subtle", label: "closed" },
};

export function ChangeStateIcon({ state }: { state: ChangeState }) {
  const s = changeStyle[state];
  const Icon = s.icon;
  return <Icon className={`size-4 shrink-0 ${s.cls} ${state === "queued" ? "animate-pulse motion-reduce:animate-none" : ""}`} aria-label={s.label} />;
}

export function ChangeStateBadge({ state }: { state: ChangeState }) {
  const s = changeStyle[state];
  return (
    <span className={`inline-flex items-center gap-1.5 rounded-full border border-line px-2 py-0.5 text-[12px] ${s.cls}`}>
      <ChangeStateIcon state={state} />
      {state === "queued" ? <span className="shimmer">{s.label}</span> : s.label}
    </span>
  );
}

export function CheckIcon({ status }: { status: CheckStatus }) {
  if (status === "success") return <CircleCheck className="size-3.5 shrink-0 text-ok" aria-label="passed" />;
  if (status === "failure") return <CircleX className="size-3.5 shrink-0 text-danger" aria-label="failed" />;
  if (status === "running")
    return <LoaderCircle className="size-3.5 shrink-0 animate-spin text-accent motion-reduce:animate-none" aria-label="running" />;
  return <CircleDashed className="size-3.5 shrink-0 text-subtle" aria-label={status} />;
}

/** `3 ✓ · 1 ✗ · 2 running` in one line. */
export function ChecksSummary({ checks }: { checks: Check[] }) {
  if (!checks.length) return <span className="text-subtle">no checks</span>;
  const n = (s: CheckStatus[]) => checks.filter((c) => s.includes(c.status)).length;
  const ok = n(["success", "skipped"]);
  const bad = n(["failure", "cancelled"]);
  const run = n(["running", "queued"]);
  return (
    <span className="inline-flex items-center gap-2 font-mono text-[11px]">
      {ok > 0 && (
        <span className="inline-flex items-center gap-1 text-ok">
          <CircleCheck className="size-3" /> {ok}
        </span>
      )}
      {bad > 0 && (
        <span className="inline-flex items-center gap-1 text-danger">
          <CircleX className="size-3" /> {bad}
        </span>
      )}
      {run > 0 && (
        <span className="inline-flex items-center gap-1 text-accent">
          <LoaderCircle className="size-3 animate-spin motion-reduce:animate-none" /> {run}
        </span>
      )}
    </span>
  );
}

const entryWords: Record<QueueInfo["state"], string> = {
  waiting: "waiting",
  testing: "testing",
  bisecting: "bisecting",
  landing: "landing",
  ejected: "ejected",
  landed: "landed",
};

/** `queue · #3 · urgent` while queued, or how its last time in the queue ended. */
export function QueuePill({ queue }: { queue: QueueInfo }) {
  const busy = queue.state === "testing" || queue.state === "bisecting" || queue.state === "landing";
  const tone = queue.state === "ejected" ? "text-danger" : queue.state === "landed" ? "text-ok" : busy ? "text-accent" : "text-muted";
  return (
    <span className={`inline-flex items-center gap-1.5 rounded-full border border-line px-2 py-0.5 text-[12px] ${tone}`}>
      <ListOrdered className="size-3.5" />
      {busy ? <span className="shimmer">{entryWords[queue.state]}</span> : entryWords[queue.state]}
      {queue.position != null && <span className="font-mono text-[11px] text-subtle">#{queue.position}</span>}
      {queue.lane === "urgent" && <span className="font-mono text-[11px] text-subtle">urgent</span>}
    </span>
  );
}

const blockerIcon: Record<string, LucideIcon> = {
  draft: FileClock,
  untrusted: ShieldQuestion,
  checks: CircleDashed,
};

/**
 * Why a change can't be queued or land yet, one row per blocker. `action` renders the button that
 * clears one (mark ready, approve the run) where the viewer is allowed to.
 */
export function BlockerList({ blockers, action }: { blockers: Blocker[]; action?: (kind: BlockerAction) => ReactNode }) {
  if (!blockers.length) return null;
  return (
    <ul className="space-y-2.5">
      {blockers.map((b, i) => {
        const copy = blockerCopy(b);
        const Icon = blockerIcon[b.kind] ?? CircleX;
        const button = copy.action && action?.(copy.action);
        return (
          <li key={`${b.kind}-${i}`} className="flex items-start gap-2.5">
            <Icon className="mt-0.5 size-3.5 shrink-0 text-subtle" />
            <div className="min-w-0 flex-1">
              <p className="text-[13px]">{copy.title}</p>
              {copy.detail && <p className="text-[12px] text-subtle">{copy.detail}</p>}
              {button && <div className="mt-2">{button}</div>}
            </div>
          </li>
        );
      })}
    </ul>
  );
}

/** `github#123` (linked when we know the URL) or `native`. */
export function SourceBadge({ source, url }: { source: string; url: string | null }) {
  const github = source.startsWith("github");
  const body = (
    <>
      {github && <GitHubMark className="size-3" />}
      {github ? source.replace(/^github/, "") : source}
    </>
  );
  const cls = "inline-flex items-center gap-1 rounded border border-line px-1.5 py-px font-mono text-[10px] text-subtle";
  return url ? (
    <a href={url} target="_blank" rel="noreferrer" className={`${cls} hover:border-line-2 hover:text-fg`} onClick={(e) => e.stopPropagation()}>
      {body}
    </a>
  ) : (
    <span className={cls}>{body}</span>
  );
}

/** A clock that re-renders its users every second (lease countdowns, "ago"). */
export function useNow(intervalMs = 1000) {
  const [now, setNow] = useState(() => Date.now() / 1000);
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now() / 1000), intervalMs);
    return () => clearInterval(t);
  }, [intervalMs]);
  return now;
}

export function formatLease(secondsLeft: number) {
  if (secondsLeft <= 0) return "expired";
  if (secondsLeft < 60) return `${Math.ceil(secondsLeft)}s`;
  if (secondsLeft < 3600) return `${Math.floor(secondsLeft / 60)}m ${Math.floor(secondsLeft % 60)}s`;
  return `${Math.floor(secondsLeft / 3600)}h ${Math.floor((secondsLeft % 3600) / 60)}m`;
}

const FEED_ROWS = 40;
const RATE_WINDOW_S = 5;
/** At 40k merges a day the feed never stops; the marks celebrate at most this often. */
const MARK_EVERY_MS = 8000;
/** A busy queue lands a batch every few seconds; the mark rises for it at most this often. */
const QUEUE_MARK_EVERY_MS = 12_000;

export interface RatePoint {
  t: number;
  merges: number;
}

/**
 * A repo's live event feed. Landed events are buffered and flushed four times a second, so a
 * burst of thousands of merges costs a handful of renders; the rate is sampled every second over
 * a short moving window. Other events (changes, work, checks, previews, reviews, threads, pushes) just tell
 * the page to refetch (`onOther`), debounced.
 * The shared mark plays `success` for landings, throttled and only when it isn't busy.
 */
export function useRepoEvents(
  repo: string | null,
  opts: { fallbackRate?: number; onOther?: () => void; onQueue?: (e: QueueEvent) => void } = {},
) {
  const { state, play } = useMarkState();
  const [feed, setFeed] = useState<LandedEvent[]>([]);
  const [rate, setRate] = useState<RatePoint[]>([]);
  const [open, setOpen] = useState(false);

  const live = useRef({ state, play, fallback: opts.fallbackRate, onOther: opts.onOther, onQueue: opts.onQueue, open: false });
  useEffect(() => {
    live.current = { state, play, fallback: opts.fallbackRate, onOther: opts.onOther, onQueue: opts.onQueue, open };
  });

  useEffect(() => {
    if (!repo) return;
    const buffer: LandedEvent[] = [];
    const recent: number[] = [];
    let lastMark = 0;
    let lastQueueMark = 0;
    let other: ReturnType<typeof setTimeout> | undefined;
    const bump = () => {
      clearTimeout(other);
      other = setTimeout(() => live.current.onOther?.(), 400);
    };

    const unsubscribe = subscribe(repo, {
      landed: (e) => {
        buffer.push(e);
        recent.push(Date.now());
      },
      change: bump,
      work: bump,
      check: bump,
      preview: bump,
      review: bump,
      thread: bump,
      push: bump,
      queue: (e) => {
        live.current.onQueue?.(e);
        bump();
        // A batch landing is a deploy: the marks rise, then celebrate. Throttled, and only from idle.
        const { state: s, play: p } = live.current;
        if (shouldPlayLanding(e, s, Date.now(), lastQueueMark, QUEUE_MARK_EVERY_MS)) {
          lastQueueMark = Date.now();
          p([
            ["deploying", 1600],
            ["success", 1000],
          ]);
        }
      },
      open: setOpen,
    });

    const flush = setInterval(() => {
      if (!buffer.length) return;
      const batch = buffer.splice(0);
      setFeed((f) => prependBatch(f, batch, FEED_ROWS));
      const { state: s, play: p } = live.current;
      if (s === "idle" && mayPulse(Date.now(), lastMark, MARK_EVERY_MS)) {
        lastMark = Date.now();
        p([["success", 1200]]);
      }
    }, 250);

    const sample = setInterval(() => {
      const now = Date.now();
      prune(recent, now, RATE_WINDOW_S);
      const merges = live.current.open || live.current.fallback == null ? windowRate(recent.length, RATE_WINDOW_S) : live.current.fallback;
      setRate((r) => [...r, { t: now / 1000, merges }].slice(-300));
    }, 1000);

    return () => {
      unsubscribe();
      clearInterval(flush);
      clearInterval(sample);
      clearTimeout(other);
      setFeed([]);
      setRate([]);
      setOpen(false);
    };
  }, [repo]);

  const current = rate.length ? rate[rate.length - 1].merges : (opts.fallbackRate ?? 0);
  return { feed, rate, current, open };
}

export { sortChanges } from "@/lib/code-api";
