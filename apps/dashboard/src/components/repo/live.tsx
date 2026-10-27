"use client";

import { GitMerge } from "lucide-react";
import Link from "next/link";
import { createContext, useContext, useEffect, useRef, useState, type ReactNode } from "react";
import { formatCount } from "@/lib/redis-api";
import { firstLine, formatMergeRate, shortId, subscribe, type ChangesetSummary, type LandedEvent } from "@/lib/code-api";
import { mayPulse, measuredRate, prependBatch, prune } from "@/lib/live-rate";
import { queueStripText, summaryFromEvent, summaryFromStats, type QueueSummary } from "@/lib/queue";
import { ago } from "@/lib/time";
import { useMarkState } from "../mark-state";

interface Live {
  mergesToday: number;
  rate: number;
  /** Bumps on a landing, at most every PULSE_MS: the key of the pulse animation. */
  pulse: number;
  /** Newest first. */
  landed: LandedEvent[];
  connected: boolean;
  /** Merge-queue repos only: what the queue is doing, live. */
  queue: QueueSummary | null;
}

const Ctx = createContext<Live | null>(null);

const PULSE_MS = 1200;
const MARK_EVERY_MS = 10_000;
const RATE_WINDOW_S = 10;
/** Shortest span a rate is averaged over, so the first landing doesn't read as a spike. */
const MIN_SPAN_S = 3;
const KEEP = 12;

/**
 * One event stream per repo page, shared by the live strip and the landings feed. Events are
 * buffered and flushed four times a second, so thousands of merges cost a handful of renders.
 */
export function RepoLive({
  repo,
  mergesToday,
  rate,
  queueDepth,
  queueBatch = 0,
  children,
}: {
  repo: string;
  mergesToday: number;
  rate: number;
  /** Set for merge-queue repos: the depth to show until the first queue event arrives. */
  queueDepth?: number | null;
  /** The batch in flight when the page rendered, so the strip shows it before the first queue event. */
  queueBatch?: number;
  children: ReactNode;
}) {
  const { state, play } = useMarkState();
  const [live, setLive] = useState<Live>({
    mergesToday,
    rate,
    pulse: 0,
    landed: [],
    connected: false,
    queue: queueDepth == null ? null : summaryFromStats({ queue_depth: queueDepth, queue_batch: queueBatch }),
  });
  const mark = useRef({ state, play });
  useEffect(() => {
    mark.current = { state, play };
  });

  useEffect(() => {
    const buffer: LandedEvent[] = [];
    const times: number[] = [];
    const openedAt = Date.now();
    let lastPulse = 0;
    let lastMark = 0;
    const unsubscribe = subscribe(repo, {
      landed: (e) => {
        buffer.push(e);
        times.push(Date.now());
      },
      queue: (e) => setLive((l) => (l.queue ? { ...l, queue: summaryFromEvent(e) } : l)),
      open: (connected) => setLive((l) => ({ ...l, connected })),
    });
    const flush = setInterval(() => {
      const now = Date.now();
      prune(times, now, RATE_WINDOW_S);
      // Until something lands or a full window has passed, the server's rate is the better
      // estimate; after that, what we've seen over the last window.
      const measured = measuredRate(times.length, (now - openedAt) / 1000, RATE_WINDOW_S, MIN_SPAN_S);
      if (!buffer.length) {
        if (measured != null) setLive((l) => (l.rate === measured ? l : { ...l, rate: measured }));
        return;
      }
      const batch = buffer.splice(0);
      const pulse = mayPulse(now, lastPulse, PULSE_MS);
      if (pulse) lastPulse = now;
      setLive((l) => ({
        ...l,
        mergesToday: l.mergesToday + batch.length,
        rate: measured ?? l.rate,
        pulse: pulse ? l.pulse + 1 : l.pulse,
        landed: prependBatch(l.landed, batch, KEEP),
      }));
      const { state: s, play: p } = mark.current;
      if (s === "idle" && now - lastMark > MARK_EVERY_MS) {
        lastMark = now;
        p([["success", 1200]]);
      }
    }, 250);
    return () => {
      unsubscribe();
      clearInterval(flush);
    };
  }, [repo]);

  return <Ctx.Provider value={live}>{children}</Ctx.Provider>;
}

function useLive() {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error("useLive must be used inside <RepoLive>");
  return ctx;
}

/**
 * "keep the code rolling — N merges today · x.x/s", with one small accent pulse per landing; on a
 * merge-queue repo also "12 in queue · landing batch of 8".
 */
export function LiveStrip() {
  const { mergesToday, rate, pulse, connected, queue } = useLive();
  const queued = queueStripText(queue);
  return (
    <p className="flex items-center gap-2.5 text-[12px] text-subtle" aria-live="off">
      <span className="relative inline-flex size-1.5 shrink-0">
        <span className={`absolute inset-0 rounded-full ${connected ? "bg-accent" : "bg-line-2"}`} />
        {pulse > 0 && <span key={pulse} className="landing-pulse absolute inset-0 rounded-full bg-accent opacity-0" />}
      </span>
      <span>keep the code rolling</span>
      <span className="text-line-2">—</span>
      <span className="font-mono tabular-nums text-muted">{formatCount(mergesToday)}</span>
      <span>{mergesToday === 1 ? "merge" : "merges"} today</span>
      <span className="text-line-2">·</span>
      <span className="font-mono tabular-nums text-muted">{formatMergeRate(rate)}</span>
      {queued && (
        <>
          <span className="text-line-2">·</span>
          <span className="font-mono tabular-nums text-muted">{queued}</span>
        </>
      )}
    </p>
  );
}

/** Latest landings: seeded from the server's log, then live. */
export function LatestLandings({ repo, initial, now }: { repo: string; initial: ChangesetSummary[]; now: number }) {
  const { landed } = useLive();
  const [clock, setClock] = useState(now);
  useEffect(() => {
    const t = setInterval(() => setClock(Date.now() / 1000), 30_000);
    return () => clearInterval(t);
  }, []);

  const seen = new Set(landed.map((e) => e.changeset));
  const rows = [
    ...landed.map((e) => ({ id: e.changeset, message: e.message, author: e.author, at: e.at, fresh: true })),
    ...initial
      .filter((c) => !seen.has(c.id))
      .map((c) => ({ id: c.id, message: c.message, author: c.author.name, at: c.author.time, fresh: false })),
  ].slice(0, 8);

  if (!rows.length) return <p className="text-[12px] text-subtle">Nothing has landed yet.</p>;
  return (
    <ul className="space-y-3">
      {rows.map((r) => (
        <li key={r.id} className={`flex gap-2.5 ${r.fresh ? "rise" : ""}`}>
          <GitMerge className={`mt-0.5 size-3.5 shrink-0 ${r.fresh ? "text-ok" : "text-subtle"}`} />
          <div className="min-w-0">
            <Link href={`/${repo}/changeset/${r.id}`} className="line-clamp-2 text-[13px] leading-snug hover:text-accent">
              {firstLine(r.message) || "(no message)"}
            </Link>
            <p className="mt-0.5 truncate font-mono text-[11px] text-subtle">
              {shortId(r.id).slice(0, 10)} · {r.author} · {ago(r.at, clock)}
            </p>
          </div>
        </li>
      ))}
    </ul>
  );
}
