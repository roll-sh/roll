"use client";

import { Check, ChevronDown, Sparkles } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { codeApi, type AiJob, type AiModeSetting, type Change, type ReviewAi } from "@/lib/code-api";
import {
  activeJob,
  aiHeader,
  jobSteps,
  lastFailure,
  markStep,
  MODE_HINT,
  severityTone,
  type Severity,
} from "@/lib/ai-review";
import { useMarkState } from "../mark-state";
import { btn } from "../ui";

// ── jobs ──

/**
 * roll's jobs on a change: polled every few seconds while one is queued or running (and once on
 * `reload`). While roll works the shared mark thinks; it plays `success` once when the review lands.
 */
export function useAiJobs(changeId: string | null, onFinished?: () => void) {
  const [jobs, setJobs] = useState<AiJob[] | null>(null);
  const { state, setState, play } = useMarkState();
  const live = useRef({ state, setState, play, onFinished, active: false });
  useEffect(() => {
    live.current = { ...live.current, state, setState, play, onFinished };
  });

  const reload = useCallback(async () => {
    if (!changeId) return;
    try {
      const list = await codeApi.aiJobs(changeId);
      setJobs(list);
      const now = activeJob(list) !== null;
      const l = live.current;
      const step = markStep(l.active, now, list[list.length - 1] ?? null, l.state);
      if (step === "think") l.setState("thinking");
      else if (step === "success") l.play([["success", 1200]]);
      else if (step === "idle") l.setState("idle");
      if (l.active && !now) l.onFinished?.();
      l.active = now;
    } catch {
      // Older servers without the reviewer: nothing to show.
      setJobs([]);
    }
  }, [changeId]);

  const active = activeJob(jobs);
  useEffect(() => {
    reload();
  }, [reload]);
  useEffect(() => {
    if (!active) return;
    const t = setInterval(reload, 3000);
    return () => clearInterval(t);
  }, [active, reload]);

  // Leaving the page mid-review hands the mark back.
  useEffect(
    () => () => {
      if (live.current.active && live.current.state === "thinking") live.current.setState("idle");
    },
    [],
  );

  return { jobs, active, failure: lastFailure(jobs), reload };
}

/** "roll is reviewing…": the job's real stages as step lines that shimmer, like the dashboard's assistant. */
export function AiProgress({ job }: { job: AiJob }) {
  const { steps, current, fraction } = jobSteps(job);
  return (
    <div className="space-y-2">
      <ol className="rise space-y-1.5" aria-live="polite">
        {steps.map((s, i) =>
          i > current ? null : (
            <li key={i} className="rise flex items-center gap-2 text-[12px]">
              {i < current ? <Check className="size-3.5 text-ok" strokeWidth={3} /> : <span className="size-1.5 rounded-full bg-accent" />}
              <span className={i < current ? "text-muted" : "shimmer"}>{s}</span>
            </li>
          ),
        )}
      </ol>
      {fraction != null && (
        <span className="relative block h-0.5 w-full overflow-hidden rounded-full bg-line" aria-hidden>
          <span className="absolute inset-y-0 left-0 bg-accent transition-[width] duration-300 ease-out" style={{ width: `${Math.round(fraction * 100)}%` }} />
        </span>
      )}
    </div>
  );
}

const MODES: AiModeSetting[] = ["auto", "standard", "deep", "max"];

/** "Run AI review ▾" for writers: pick a depth, optionally review the whole change again. */
export function RunAiReview({
  change,
  active,
  busy,
  run,
}: {
  change: Pick<Change, "id">;
  active: AiJob | null;
  busy: string | null;
  run: (key: string, fn: () => Promise<unknown>) => Promise<unknown>;
}) {
  const [open, setOpen] = useState(false);
  const [full, setFull] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => !ref.current?.contains(e.target as Node) && setOpen(false);
    const esc = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", close);
    document.addEventListener("keydown", esc);
    return () => {
      document.removeEventListener("mousedown", close);
      document.removeEventListener("keydown", esc);
    };
  }, [open]);

  const start = async (mode: AiModeSetting) => {
    setOpen(false);
    await run("ai-review", () => codeApi.runAiReview(change.id, { mode, full: full || undefined }));
  };

  return (
    <div ref={ref} className="relative">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        disabled={busy !== null || active !== null}
        className={btn}
        aria-haspopup="menu"
        aria-expanded={open}
      >
        <Sparkles className="size-3.5" />
        {active ? <span className="shimmer">roll is reviewing…</span> : busy === "ai-review" ? <span className="shimmer">Starting…</span> : "Run AI review"}
        <ChevronDown className="size-3" />
      </button>
      {open && (
        <div role="menu" className="rise absolute right-0 z-20 mt-1 w-72 rounded-xl border border-line bg-panel p-1.5 text-[12px]">
          {MODES.map((m) => (
            <button
              key={m}
              role="menuitem"
              type="button"
              onClick={() => start(m)}
              className="flex w-full flex-col items-start rounded-lg px-2.5 py-1.5 text-left transition hover:bg-panel-2"
            >
              <span className="text-fg capitalize">{m}</span>
              <span className="text-[11px] text-subtle">{MODE_HINT[m]}</span>
            </button>
          ))}
          <label className="mt-1 flex items-center gap-2 border-t border-line px-2.5 pt-2 pb-1 text-[11px] text-subtle">
            <input type="checkbox" checked={full} onChange={(e) => setFull(e.target.checked)} className="accent-[var(--accent)]" />
            Full review — not just what changed since roll&apos;s last review
          </label>
        </div>
      )}
    </div>
  );
}

// ── a roll review ──

/** The top of a roll review: mode and why, speed, model, cost, and what it skipped. */
export function AiReviewHeader({ ai }: { ai: ReviewAi }) {
  const h = aiHeader(ai);
  return (
    <div className="space-y-1.5 rounded-lg border border-line bg-panel-2/40 px-3 py-2 text-[12px]">
      <p className="flex flex-wrap items-center gap-x-2 gap-y-1">
        <span className="text-fg">{h.mode}</span>
        {h.incremental && <span className="rounded-full border border-line px-1.5 py-px text-[10px] text-subtle">changes since last review</span>}
        <span className="text-subtle">·</span>
        <span className="text-muted">{h.speed}</span>
        <span className="text-subtle">·</span>
        <span className="font-mono text-[11px] text-muted">{h.model}</span>
        <span className="text-subtle">·</span>
        <span className="text-muted">{h.key}</span>
        <span className="ml-auto font-mono text-[11px] text-muted" title={h.tokens}>
          {h.price}
          {h.listPrice && <span className="ml-1 text-subtle line-through">{h.listPrice}</span>}
        </span>
      </p>
      {h.auto && h.reasons.length > 0 ? (
        <details className="group">
          <summary className="cursor-pointer list-none text-[11px] text-subtle hover:text-fg">
            {h.chosen} because {h.reasons[0]}
            {h.reasons.length > 1 && <span className="group-open:hidden"> · {h.reasons.length - 1} more</span>}
          </summary>
          {h.reasons.length > 1 && (
            <ul className="mt-1 list-disc space-y-0.5 pl-4 text-[11px] text-muted">
              {h.reasons.slice(1).map((r) => (
                <li key={r}>{r}</li>
              ))}
            </ul>
          )}
        </details>
      ) : (
        <p className="text-[11px] text-subtle">{h.chosen}</p>
      )}
      <p className="text-[11px] text-subtle">
        {h.files} file{h.files === 1 ? "" : "s"} reviewed · {h.findings} finding{h.findings === 1 ? "" : "s"}
        {h.dropped > 0 && ` · ${h.dropped} dropped as unverified or duplicate`} · {h.tokens}
      </p>
      {h.skipped.length > 0 && (
        <details>
          <summary className="cursor-pointer text-[11px] text-subtle hover:text-fg">
            {h.skipped.length} file{h.skipped.length === 1 ? "" : "s"} not reviewed
          </summary>
          <ul className="mt-1 space-y-0.5 text-[11px]">
            {h.skipped.map((s) => (
              <li key={s.path} className="flex gap-2">
                <span className="truncate font-mono text-muted">{s.path}</span>
                <span className="shrink-0 text-subtle">{s.reason}</span>
              </li>
            ))}
          </ul>
        </details>
      )}
    </div>
  );
}

/** bug · security · perf · style · nit — monochrome; bugs and security findings carry the accent. */
export function SeverityChip({ severity }: { severity: Severity }) {
  const accent = severityTone(severity) === "accent";
  return (
    <span
      className={`inline-flex items-center rounded-full border px-1.5 py-px font-mono text-[10px] ${
        accent ? "border-accent/40 text-accent" : "border-line text-subtle"
      }`}
    >
      {severity}
    </span>
  );
}

/** Confidence for deep and max findings: a quiet hairline meter. */
export function Confidence({ value }: { value: number }) {
  const pct = Math.round(value * 100);
  return (
    <span className="inline-flex items-center gap-1.5 text-[10px] text-subtle" title={`roll is ${pct}% confident in this finding`}>
      <span className="relative h-0.5 w-10 overflow-hidden rounded-full bg-line" aria-hidden>
        <span className="absolute inset-y-0 left-0 bg-muted" style={{ width: `${pct}%` }} />
      </span>
      {pct}%
    </span>
  );
}
