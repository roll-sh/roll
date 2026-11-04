"use client";

/**
 * Runs: an automation's history and one run's detail, live. The detail is T3's work log turned
 * into a timeline (GitHub Actions' steps, n8n's input/output): each step's state, timing and
 * attempt, its log lines and output when opened, the threads it started (open them beside the
 * canvas), and Retry or Cancel. `automation_run` events move it while you watch.
 */

import { ArrowLeft, ChevronRight, CircleAlert, CircleStop, MessageSquare, RotateCcw } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { isRunFinal, type AutomationEvent, type AutomationRun, type StepRun } from "@/lib/automations-api";
import { canCancel, canRetry, duration, stepInfo, stepLabel, took } from "@/lib/automations-ui";
import { setAgentsView } from "@/lib/agents-surface";
import { cn } from "../ui";
import { useAgents } from "../use-agents";
import { RunRows } from "./automations-view";
import { QuietButton, RunStatus, StepGlyph, stepIcon, useAutomations } from "./shared";

/** One automation's runs, newest first. */
export function RunHistory({ automation }: { automation: string }) {
  const { runs, store } = useAutomations();
  const [now] = useState(() => Date.now());
  const [loaded, setLoaded] = useState(false);
  const [end, setEnd] = useState(false);
  const mine = useMemo(() => [...runs.values()].filter((r) => r.automation === automation).sort((a, b) => b.created_at - a.created_at), [runs, automation]);
  // Its own page once, so an automation with older runs than the shared first page shows them.
  useEffect(() => {
    if (!store || loaded) return;
    setLoaded(true);
    void store.more(automation, undefined).then((p) => p.length < 50 && setEnd(true), () => setEnd(true));
  }, [store, automation, loaded]);
  if (!mine.length) return <p className="py-16 text-center text-[13px] text-subtle">No runs yet. Press Run, or wait for its trigger.</p>;
  return (
    <div className="@container">
      <RunRows runs={mine} showName={false} now={now} onOpen={(r) => setAgentsView({ kind: "automations", open: automation, tab: "runs", run: r.id })} />
      {!end && mine.length >= 50 && (
        <div className="mt-3 flex justify-center">
          <QuietButton onClick={() => void store?.more(automation, mine.at(-1)?.id).then((p) => p.length < 50 && setEnd(true))}>Older runs</QuietButton>
        </div>
      )}
    </div>
  );
}

/** Ticks once a second while `on`, for live durations. */
function useNow(on: boolean): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!on) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [on]);
  return now;
}

export function RunDetail({ run: id, onBack }: { run: string; onBack: () => void }) {
  const { runs, store, catalog } = useAutomations();
  const { client, openThread } = useAgents();
  const [fetched, setFetched] = useState<{ run: AutomationRun; event: AutomationEvent } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [acting, setActing] = useState<"retry" | "cancel" | null>(null);
  useEffect(() => {
    let live = true;
    setFetched(null);
    store?.api.runDetail(store.owner, id).then(
      (d) => {
        if (!live) return;
        setFetched(d);
        store.putRun(d.run);
      },
      (e: Error) => live && setError(e.message),
    );
    return () => {
      live = false;
    };
  }, [store, id]);
  // The store's copy follows the stream; the fetched one only fills in until it's there.
  const run = runs.get(id) ?? fetched?.run ?? null;
  const now = useNow(!!run && !isRunFinal(run.status));
  const [opened, setOpened] = useState<Set<string>>(new Set());
  const [showEvent, setShowEvent] = useState(false);
  if (!run) return <p className="py-16 text-center text-[13px] text-subtle">{error ?? <span className="shimmer">Loading the run…</span>}</p>;
  const total = took(run.started_at ?? run.created_at, run.finished_at, now);
  const act = async (what: "retry" | "cancel") => {
    if (!store) return;
    setActing(what);
    setError(null);
    try {
      store.putRun(what === "retry" ? await store.api.retry(store.owner, run.id) : await store.api.cancel(store.owner, run.id));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setActing(null);
    }
  };
  const isOpen = (s: StepRun) => opened.has(s.id) !== (s.status === "running" || s.status === "waiting" || s.status === "retrying" || s.status === "failed");
  const toggle = (s: StepRun) => setOpened((o) => {
    const n = new Set(o);
    if (n.has(s.id)) n.delete(s.id);
    else n.add(s.id);
    return n;
  });
  // The conversation opens beside the canvas, and the surface reveals its card there.
  const openOnCanvas = (thread: string) => openThread(thread);
  return (
    <div className="fade-in" aria-label={`Run ${run.id}`}>
      <div className="mb-4 flex items-center gap-2">
        <button type="button" onClick={onBack} className="inline-flex h-7 items-center gap-1 rounded-md pr-2 pl-1 text-[12.5px] text-muted transition-colors hover:bg-fg/[0.05] hover:text-fg">
          <ArrowLeft className="size-3.5" /> Runs
        </button>
        <span className="ml-auto flex items-center gap-1.5">
          {canCancel(run) && (
            <QuietButton onClick={() => void act("cancel")} disabled={!!acting}>
              <CircleStop /> {acting === "cancel" ? "Cancelling…" : "Cancel"}
            </QuietButton>
          )}
          {canRetry(run) && (
            <QuietButton onClick={() => void act("retry")} disabled={!!acting}>
              <RotateCcw /> {acting === "retry" ? "Retrying…" : "Retry"}
            </QuietButton>
          )}
        </span>
      </div>
      <div className="rounded-xl border border-line-2 bg-panel p-3.5 shadow-card inset-shadow-highlight">
        <div className="flex items-center gap-2">
          <RunStatus status={run.status} className="text-[12.5px] font-medium" />
          <span className="text-faint">·</span>
          <span className="font-mono text-[11.5px] text-subtle tabular-nums">{total === null ? "not started" : duration(total)}</span>
          <span className="ml-auto text-[11.5px] text-subtle tabular-nums" title={new Date(run.created_at).toLocaleString()}>
            {new Date(run.created_at).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })}
          </span>
        </div>
        <p className="mt-2 text-[13.5px] font-[510] tracking-[-0.01em]">{run.event.summary}</p>
        <p className="mt-0.5 text-[12px] text-subtle">
          {run.event.source} · {run.event.kind}
          {" · "}
          <button type="button" onClick={() => setShowEvent(!showEvent)} className="text-muted underline-offset-2 hover:text-fg hover:underline" aria-expanded={showEvent}>
            {showEvent ? "Hide the event" : "Show the event"}
          </button>
        </p>
        {showEvent && <Json value={fetched?.event.data ?? null} className="mt-2.5" />}
        {run.reason && run.status !== "succeeded" && (
          <p className={cn("mt-2.5 flex items-start gap-1.5 text-[12px]", run.status === "failed" ? "text-danger" : "text-muted")}>
            <CircleAlert className="mt-0.5 size-3.5 shrink-0" /> {run.reason}
          </p>
        )}
        {error && <p className="mt-2 text-[12px] text-danger">{error}</p>}
      </div>

      <ol className="relative mt-4" aria-label="Steps">
        {run.steps.map((s, i) => {
          const info = stepInfo(catalog, s.kind);
          const ms = took(s.started_at, s.finished_at, now);
          const threads = run.threads.filter((t) => t.step === s.id);
          const open = isOpen(s);
          const Icon = stepIcon(s.kind);
          const live = s.status === "running" || s.status === "waiting" || s.status === "retrying";
          return (
            <li key={s.id} className="relative pl-7">
              {i < run.steps.length - 1 && <span className={cn("absolute top-[22px] bottom-0 left-[8.5px] w-px", s.status === "succeeded" ? "bg-ok/30" : "bg-line-2")} aria-hidden />}
              <span className="absolute top-[7px] left-0">
                <StepGlyph status={s.status} />
              </span>
              <button type="button" onClick={() => toggle(s)} aria-expanded={open} className="group/step flex min-h-8 w-full items-center gap-2 rounded-md py-1 pr-1 text-left">
                <Icon className="size-3.5 shrink-0 text-subtle" strokeWidth={1.75} />
                <span className={cn("min-w-0 truncate text-[13px]", s.status === "pending" || s.status === "skipped" ? "text-subtle" : "text-fg", live && "shimmer")}>{info?.title ?? s.kind}</span>
                <code className="shrink-0 font-mono text-[10.5px] text-faint">{s.id}</code>
                {s.attempt > 1 && <span className="shrink-0 rounded-full bg-fg/[0.06] px-1.5 text-[10.5px] text-muted">try {s.attempt}</span>}
                <span className={cn("ml-auto shrink-0 text-[11.5px]", s.status === "failed" ? "text-danger" : live ? "text-accent" : "text-subtle")}>{stepLabel[s.status]}</span>
                <span className="w-14 shrink-0 text-right font-mono text-[11px] text-subtle tabular-nums">{ms === null ? "" : duration(ms)}</span>
                <ChevronRight className={cn("size-3 shrink-0 text-faint transition-transform duration-200 motion-reduce:transition-none", open && "rotate-90")} />
              </button>
              {threads.length > 0 && (
                <div className="mb-1 flex flex-wrap gap-1.5 pl-[22px]">
                  {threads.map((t) => {
                    const rec = client.data.threads.get(t.thread)?.value;
                    return (
                      <button
                        key={t.thread}
                        type="button"
                        onClick={() => openOnCanvas(t.thread)}
                        className="inline-flex h-6 max-w-full items-center gap-1.5 rounded-md border border-line-2 bg-fg/[0.03] px-2 text-[11.5px] text-soft inset-shadow-highlight transition-colors hover:border-line-3 hover:text-fg"
                        title="Open the conversation beside the canvas"
                      >
                        <MessageSquare className="size-3 shrink-0 text-subtle" />
                        <span className="truncate">{rec?.title ?? t.thread}</span>
                        {rec && <span className="shrink-0 text-subtle">· {client.data.machines.get(t.machine)?.value.name ?? ""}</span>}
                      </button>
                    );
                  })}
                </div>
              )}
              {open && (
                <div className="rise mb-3 space-y-2 pl-[22px]">
                  {s.error && <p className="text-[12px] text-danger">{s.error}</p>}
                  {s.status === "retrying" && s.next_at && <p className="text-[12px] text-muted">Trying again {Math.max(0, Math.round((s.next_at - now) / 1000))}s from now</p>}
                  {s.log.length > 0 ? (
                    <div className="max-h-96 overflow-y-auto rounded-lg border border-line bg-bg/60 px-2.5 py-2 font-mono text-[11.5px] leading-[18px]" aria-label="Log">
                      {s.log.map((l, j) => (
                        <div key={j} className={cn("flex gap-3", l.level === "error" ? "text-danger" : l.level === "warn" ? "text-soft" : "text-muted")}>
                          <span className="shrink-0 text-faint tabular-nums">+{((l.at - (s.started_at ?? l.at)) / 1000).toFixed(1)}s</span>
                          <span className="min-w-0 break-words whitespace-pre-wrap">{l.text}</span>
                        </div>
                      ))}
                    </div>
                  ) : (
                    s.status !== "pending" && <p className="text-[12px] text-subtle">No log lines.</p>
                  )}
                  {s.output != null && typeof s.output === "object" && Object.keys(s.output).length > 0 && (
                    <div>
                      <p className="mb-1 text-[11px] font-medium text-subtle">Output · steps.{s.id}</p>
                      <Json value={s.output} />
                    </div>
                  )}
                  {s.status === "pending" && <p className="text-[12px] text-subtle">Hasn&apos;t started.</p>}
                </div>
              )}
            </li>
          );
        })}
      </ol>
    </div>
  );
}

function Json({ value, className }: { value: unknown; className?: string }) {
  return <pre className={cn("max-h-72 overflow-auto rounded-lg border border-line bg-bg/60 px-2.5 py-2 font-mono text-[11.5px] leading-[18px] text-soft", className)}>{JSON.stringify(value, null, 2)}</pre>;
}
