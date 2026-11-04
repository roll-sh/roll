"use client";

/**
 * A `notify` step's notices (the `notice` stream event) as calm toasts in the surface's top-right
 * corner (T3's toasts: 360px, glass, sliding in from the edge, the newest on top). Each closes by
 * itself after a while unless the pointer is on it; its link and its run are one click away.
 */

import { CircleAlert, CircleCheck, Info, TriangleAlert, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { Notice } from "@/lib/automations-api";
import { setAgentsView, viewFromHash } from "@/lib/agents-surface";
import { cn } from "../ui";
import { useAutomationsStore } from "./shared";

/** Toasts kept at once; older ones go. */
const MAX = 3;
const STAY_MS = 7000;

export function Notices() {
  const store = useAutomationsStore();
  const [shown, setShown] = useState<Notice[]>([]);
  useEffect(() => store?.onNotice((n) => setShown((s) => [n, ...s.filter((x) => x.id !== n.id)].slice(0, MAX))), [store]);
  const dismiss = (id: string) => setShown((s) => s.filter((x) => x.id !== id));
  if (!shown.length) return null;
  return (
    <ol className="pointer-events-none absolute top-14 right-3 z-[60] flex w-[min(360px,calc(100%-1.5rem))] flex-col gap-2" aria-label="Notices" aria-live="polite">
      {shown.map((n) => (
        <Toast key={n.id} notice={n} onDismiss={() => dismiss(n.id)} />
      ))}
    </ol>
  );
}

const icons = { info: Info, success: CircleCheck, warning: TriangleAlert, error: CircleAlert };
const tones = { info: "text-muted", success: "text-ok", warning: "text-soft", error: "text-danger" };

function Toast({ notice: n, onDismiss }: { notice: Notice; onDismiss: () => void }) {
  const hover = useRef(false);
  useEffect(() => {
    const t = setInterval(() => !hover.current && Date.now() - n.at > STAY_MS && onDismiss(), 1000);
    return () => clearInterval(t);
  }, [n.at, onDismiss]);
  const Icon = icons[n.level];
  const open = () => {
    if (n.link) {
      const v = n.link.startsWith("#") ? viewFromHash(n.link) : null;
      if (v) setAgentsView(v);
      else if (/^https?:\/\//.test(n.link)) window.open(n.link, "_blank", "noopener,noreferrer");
    } else if (n.automation && n.run) setAgentsView({ kind: "automations", open: n.automation, tab: "runs", run: n.run });
    onDismiss();
  };
  const actionable = !!n.link || !!(n.automation && n.run);
  return (
    <li
      role="status"
      onPointerEnter={() => (hover.current = true)}
      onPointerLeave={() => (hover.current = false)}
      className="toast-in pointer-events-auto rounded-[10px] border border-outline glass shadow-pop inset-shadow-highlight"
    >
      <div className="flex items-start gap-2.5 py-2.5 pr-2 pl-3">
        <Icon className={cn("mt-px size-4 shrink-0", tones[n.level])} strokeWidth={1.75} aria-hidden />
        <div className="min-w-0 flex-1">
          <p className="text-[13px] leading-snug font-[510] tracking-[-0.01em]">{n.title}</p>
          {n.body && <p className="mt-0.5 line-clamp-3 text-[12px] leading-snug text-muted">{n.body}</p>}
          {actionable && (
            <button type="button" onClick={open} className="mt-1.5 text-[12px] font-medium text-accent hover:underline">
              {n.link ? "Open" : "View the run"}
            </button>
          )}
        </div>
        <button type="button" onClick={onDismiss} aria-label="Dismiss" className="grid size-6 shrink-0 place-items-center rounded-md text-subtle transition-colors hover:bg-fg/[0.06] hover:text-fg">
          <X className="size-3.5" />
        </button>
      </div>
    </li>
  );
}
