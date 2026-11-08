"use client";

/**
 * A `notify` step's notices (the `notice` stream event) as calm toasts (T3's toasts: 360px, glass,
 * sliding in from the edge) in the surface's bottom-right corner, so they never cover the side
 * panel's tools; the newest nearest the corner. Each closes by
 * itself after a while unless the pointer is on it; its link, its thread and its run are one click
 * away. The server keeps them too (`GET …/automations/notices`): the automations sheet's Notices
 * list has the ones a person missed, new ones marked until the list is seen on this device (the
 * header's automations button shows a dot meanwhile), like a notification inbox.
 */

import { CircleAlert, CircleCheck, Info, TriangleAlert, X } from "lucide-react";
import { useReducedMotion } from "@roll/brand";
import { AnimatePresence, motion, useIsPresent } from "motion/react";
import { forwardRef, useEffect, useMemo, useRef, useState } from "react";
import { DRAWER, POP_OUT, TOAST_IN } from "@/lib/agents-motion";
import type { AgentsClient } from "@/lib/agents-client";
import type { AutomationRun, AutomationsApi, Notice } from "@/lib/automations-api";
import { automationsFor, noticesOver, unseenNotices, useAutomationsState } from "@/lib/automations-store";
import { setAgentsView, viewFromHash } from "@/lib/agents-surface";
import { ago } from "../shared";
import { cn } from "../ui";
import { useAgents } from "../use-agents";
import { Named } from "./named";
import { QuietButton, useAutomations, useAutomationsStore } from "./shared";

/** What a notice opens: its link, the thread its run started (the newest), its run. */
export function noticeActions(n: Notice, run: AutomationRun | undefined, openThread: (thread: string) => void): { label: string; go: () => void }[] {
  const out: { label: string; go: () => void }[] = [];
  if (n.link) {
    const v = n.link.startsWith("#") ? viewFromHash(n.link) : null;
    if (v) out.push({ label: "Open", go: () => setAgentsView(v) });
    else if (/^https?:\/\//.test(n.link)) out.push({ label: "Open", go: () => window.open(n.link!, "_blank", "noopener,noreferrer") });
  }
  // The thread its run started, else the one it's about (the thread waiting on you).
  const thread = run?.threads.at(-1)?.thread ?? n.about?.thread;
  if (thread) out.push({ label: "Open the thread", go: () => openThread(thread) });
  if (n.automation && n.run) out.push({ label: "View the run", go: () => setAgentsView({ kind: "automations", open: n.automation!, tab: "runs", run: n.run! }) });
  return out;
}

/** Notices this device hasn't seen in the list yet (outside the agents context: its store given). */
export function useUnseenNotices(client: AgentsClient, api: AutomationsApi | undefined): number {
  const store = useMemo(() => (api ? automationsFor(client, api) : null), [client, api]);
  // A thread that waited on you and was answered since isn't news any more.
  return unseenNotices(useAutomationsState(store), noticesOver(client.data));
}

/** Every notice kept, newest first: what a person missed while nothing was open. */
export function NoticeList() {
  const { store, notices, noticesSeen, runs } = useAutomations();
  const { openThread, client } = useAgents();
  const over = noticesOver(client.data);
  const [now] = useState(() => Date.now());
  // What was new when the list opened stays marked while it's open; seeing it is seeing them.
  const [seenAt] = useState(noticesSeen);
  const [end, setEnd] = useState(false);
  useEffect(() => store?.markNoticesSeen(), [store, notices]);
  if (!notices.length)
    return <p className="py-16 text-center text-[13px] text-subtle">No notices yet. A Notify step's notices land here, and stay when nothing was open.</p>;
  return (
    <>
      <ol className="overflow-hidden rounded-xl border border-line-2 bg-panel shadow-card inset-shadow-highlight" aria-label="Notices">
        {notices.map((n) => {
          // Waited on you and was answered since: said so, quiet, not new.
          const done = over(n);
          const Icon = done ? CircleCheck : icons[n.level];
          const fresh = n.at > seenAt && !done;
          return (
            <li key={n.id} className="flex items-start gap-2.5 border-line px-3.5 py-2.5 [&+&]:border-t" aria-label={`${fresh ? "New: " : ""}${n.title}${done ? " (answered)" : ""}`}>
              <Icon className={cn("mt-px size-4 shrink-0", done ? "text-subtle" : tones[n.level])} strokeWidth={1.75} aria-hidden />
              <div className="min-w-0 flex-1">
                <p className={cn("text-[13px] leading-snug font-[510] tracking-[-0.01em]", done && "text-muted")}>{n.title}</p>
                {n.body && <p className="mt-0.5 line-clamp-2 text-[12px] leading-snug text-muted"><Named text={n.body} /></p>}
                <p className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-[12px]">
                  <span className="text-subtle tabular-nums" title={new Date(n.at).toLocaleString()}>
                    {ago(n.at, now)}
                  </span>
                  {done && <span className="text-subtle">Answered</span>}
                  {noticeActions(n, n.run ? runs.get(n.run) : undefined, openThread).map((x) => (
                    <button key={x.label} type="button" onClick={x.go} className="font-medium text-accent hover:underline">
                      {x.label}
                    </button>
                  ))}
                </p>
              </div>
              {fresh && <span className="mt-1.5 size-1.5 shrink-0 rounded-full bg-accent" aria-hidden />}
            </li>
          );
        })}
      </ol>
      {!end && notices.length >= 50 && (
        <div className="mt-3 flex justify-center">
          <QuietButton onClick={() => void store?.moreNotices().then((p) => p.length < 50 && setEnd(true), () => setEnd(true))}>Older notices</QuietButton>
        </div>
      )}
    </>
  );
}

/** Toasts kept at once; older ones go. */
const MAX = 3;
const STAY_MS = 7000;

export function Notices() {
  const store = useAutomationsStore();
  const [shown, setShown] = useState<Notice[]>([]);
  useEffect(() => store?.onNotice((n) => setShown((s) => [n, ...s.filter((x) => x.id !== n.id)].slice(0, MAX))), [store]);
  const dismiss = (id: string) => setShown((s) => s.filter((x) => x.id !== id));
  return (
    <ol className="pointer-events-none absolute right-3 bottom-16 z-[60] flex w-[min(360px,calc(100%-1.5rem))] flex-col-reverse gap-2" aria-label="Notices" aria-live="polite">
      {/* A toast that goes slides back out; the others glide into its place (they jumped). */}
      <AnimatePresence initial={false} mode="popLayout">
        {shown.map((n) => (
          <Toast key={n.id} notice={n} onDismiss={() => dismiss(n.id)} />
        ))}
      </AnimatePresence>
    </ol>
  );
}

const icons = { info: Info, success: CircleCheck, warning: TriangleAlert, error: CircleAlert };
const tones = { info: "text-muted", success: "text-ok", warning: "text-soft", error: "text-danger" };

const Toast = forwardRef<HTMLLIElement, { notice: Notice; onDismiss: () => void }>(function Toast({ notice: n, onDismiss }, ref) {
  const reduced = useReducedMotion();
  const present = useIsPresent();
  const { runs } = useAutomations();
  const { openThread, client } = useAgents();
  const over = noticesOver(client.data)(n);
  const hover = useRef(false);
  useEffect(() => {
    const t = setInterval(() => !hover.current && Date.now() - n.at > STAY_MS && onDismiss(), 1000);
    return () => clearInterval(t);
  }, [n.at, onDismiss]);
  // Answered (here or on another device) while it showed: it says nothing current any more.
  useEffect(() => {
    if (over) onDismiss();
  }, [over, onDismiss]);
  const Icon = icons[n.level];
  const actions = noticeActions(n, n.run ? runs.get(n.run) : undefined, openThread);
  return (
    <motion.li
      ref={ref}
      // Going: no longer announced, and its buttons no longer take a click.
      role={present ? "status" : undefined}
      aria-hidden={!present || undefined}
      inert={!present}
      layout={!reduced}
      initial={{ opacity: 0, x: 24 }}
      animate={{ opacity: 1, x: 0, transition: reduced ? { duration: 0 } : TOAST_IN }}
      exit={{ opacity: 0, x: 24, transition: reduced ? { duration: 0 } : POP_OUT }}
      transition={reduced ? { duration: 0 } : DRAWER}
      onPointerEnter={() => (hover.current = true)}
      onPointerLeave={() => (hover.current = false)}
      className="pointer-events-auto rounded-[10px] border border-outline glass shadow-pop inset-shadow-highlight"
    >
      <div className="flex items-start gap-2.5 py-2.5 pr-2 pl-3">
        <Icon className={cn("mt-px size-4 shrink-0", tones[n.level])} strokeWidth={1.75} aria-hidden />
        <div className="min-w-0 flex-1">
          <p className="text-[13px] leading-snug font-[510] tracking-[-0.01em]">{n.title}</p>
          {n.body && <p className="mt-0.5 line-clamp-3 text-[12px] leading-snug text-muted"><Named text={n.body} /></p>}
          {actions.length > 0 && (
            <p className="mt-1.5 flex flex-wrap gap-x-3 text-[12px]">
              {actions.map((x) => (
                <button
                  key={x.label}
                  type="button"
                  onClick={() => {
                    x.go();
                    onDismiss();
                  }}
                  className="font-medium text-accent hover:underline"
                >
                  {x.label}
                </button>
              ))}
            </p>
          )}
        </div>
        <button type="button" onClick={onDismiss} aria-label="Dismiss" className="grid size-6 shrink-0 place-items-center rounded-md text-subtle transition-colors hover:bg-fg/[0.06] hover:text-fg">
          <X className="size-3.5" />
        </button>
      </div>
    </motion.li>
  );
});
