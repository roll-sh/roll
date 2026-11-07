"use client";

/**
 * The automations sheet: everything about automations in one right sheet beside the canvas (the
 * conversation's frame and motion, `SidePanel` in agents-page.tsx), a stack you go into and back
 * out of (lib/automations-nav.ts): the list → an automation's overview → its builder or its runs →
 * one run → a conversation that run started. A page deeper in slides in from the right, one you
 * go back to from the left (`SlidePages`: the old page slides the other way and fades, inert; 200 ms;
 * static under reduced motion). The canvas stays live beside it, with a run's threads highlighted.
 */

import { ArrowLeft, Maximize2, Minimize2, PanelRightClose, X } from "lucide-react";
import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import { setAgentsView, type AutomationsView } from "@/lib/agents-surface";
import { backOf, isBuilder, levelKey, levelOf, step, type Trail } from "@/lib/automations-nav";
import { Conversation, IconButton } from "../session";
import { SlidePages } from "../slide-pages";
import { useAgents } from "../use-agents";
import { AutomationPanel } from "./automation-panel";
import { forgetListState, AutomationsView as List } from "./automations-view";
import { Overview } from "./overview";
import { RunDetail, RunHistory } from "./runs";
import { useAutomations } from "./shared";

interface Sheet {
  /** One level up: where you came from, else the parent; from the list, closes the sheet. */
  back: () => void;
  close: () => void;
  /** What Back says (the page it returns to), for its label. */
  backLabel: string | null;
  phone: boolean;
  /** The builder can fill the working area (T3's "Maximize panel"); `null` elsewhere. */
  maximized: boolean | null;
  setMaximized: (on: boolean) => void;
}

const SheetContext = createContext<Sheet | null>(null);

export function useSheet(): Sheet | null {
  return useContext(SheetContext);
}

export function AutomationsSheet({
  view,
  phone,
  maximized,
  setMaximized,
  backRef,
  onHighlight,
}: {
  view: AutomationsView;
  phone: boolean;
  maximized: boolean;
  setMaximized: (on: boolean) => void;
  /** The page's Escape calls this: one level up, and from the list, closed. */
  backRef: { current: (() => void) | null };
  /** Canvas keys to highlight beside the sheet (a run's threads, the open conversation). */
  onHighlight: (keys: readonly string[]) => void;
}) {
  // The trail follows the view (adjusted while rendering, React's way to derive from a prop).
  const [nav, setNav] = useState<{ trail: Trail; dir: -1 | 0 | 1; top: AutomationsView }>(() => ({ trail: [view], dir: 0, top: view }));
  let current = nav;
  if (nav.top !== view) {
    const s = step(nav.trail, view);
    current = { ...s, top: view };
    setNav(current);
  }
  const before = backOf(current.trail);
  const sheet = useMemo<Sheet>(
    () => ({
      back: () => setAgentsView(before ?? { kind: "canvas" }),
      close: () => setAgentsView({ kind: "canvas" }),
      backLabel: before ? labelOf(before) : null,
      phone,
      maximized: isBuilder(view) && !phone ? maximized : null,
      setMaximized,
    }),
    [before, phone, view, maximized, setMaximized],
  );
  useEffect(() => {
    backRef.current = sheet.back;
    return () => {
      backRef.current = null;
    };
  }, [backRef, sheet.back]);

  const level = levelOf(view);
  const { runs } = useAutomations();
  const run = view.run ? runs.get(view.run) : undefined;
  const highlight = useMemo(() => (view.thread ? [`t:${view.thread}`] : run ? [...new Set(run.threads.map((t) => `t:${t.thread}`))] : []), [view.thread, run]);
  const highlightKey = highlight.join();
  useEffect(() => {
    onHighlight(highlight);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [highlightKey, onHighlight]);
  useEffect(() => () => onHighlight([]), [onHighlight]);
  useEffect(() => forgetListState, []);

  const page =
    level.kind === "thread" ? (
      <ThreadPage thread={level.thread} />
    ) : level.kind === "run" ? (
      <RunDetail run={level.run} onThread={(thread) => setAgentsView({ ...view, tab: view.open ? "runs" : view.tab, thread })} />
    ) : level.kind === "history" ? (
      <RunHistory automation={level.id} list={view.list} />
    ) : level.kind === "builder" ? (
      <AutomationPanel key={`${view.open}:${view.template ?? ""}`} view={{ ...view, open: level.id }} />
    ) : level.kind === "automation" ? (
      <Overview id={level.id} list={view.list} />
    ) : (
      <List view={view} phone={phone} />
    );
  return (
    <SheetContext.Provider value={sheet}>
      {/* A level deeper slides in from the right, Back from the left; the page left behind slides
          the other way and fades, inert (it was gone at once). A tab on the same level stays put. */}
      <SlidePages page={levelKey(level)} direction={current.dir} className="flex min-h-0 flex-1 flex-col">
        <div className="flex min-h-0 flex-1 flex-col" data-level={level.kind}>
          {page}
        </div>
      </SlidePages>
    </SheetContext.Provider>
  );
}

function labelOf(v: AutomationsView): string {
  const l = levelOf(v);
  switch (l.kind) {
    case "list":
      return v.list === "runs" ? "Runs" : v.list === "templates" ? "Templates" : v.list === "notices" ? "Notices" : "Automations";
    case "automation":
      return "Overview";
    case "builder":
      return "Edit";
    case "history":
      return "Runs";
    case "run":
      return "Run";
    case "thread":
      return "Conversation";
  }
}

/** Back, one level up (a labelled arrow); at the list on a phone it closes the sheet. */
export function BackButton() {
  const s = useSheet();
  if (!s || (!s.backLabel && !s.phone)) return null;
  const label = s.backLabel ? `Back to ${s.backLabel.toLowerCase()} (Esc)` : "Back to the canvas";
  return (
    <button
      type="button"
      onClick={s.back}
      aria-label={label}
      title={label}
      className="-ml-1 inline-flex h-7 shrink-0 items-center gap-1 rounded-md pr-1.5 pl-1 text-[12.5px] text-muted transition-colors duration-150 hover:bg-fg/[0.06] hover:text-fg"
    >
      <ArrowLeft className="size-4" strokeWidth={1.75} />
      {s.backLabel && <span className="max-w-24 truncate max-sm:sr-only">{s.backLabel}</span>}
    </button>
  );
}

/** Maximize (the builder) and Close: the end of every page's header. */
export function SheetTools() {
  const s = useSheet();
  if (!s) return null;
  return (
    <span className="flex shrink-0 items-center text-muted">
      {s.maximized !== null && (
        <IconButton label={s.maximized ? "Restore the sheet's size" : "Maximize the sheet"} active={s.maximized} onClick={() => s.setMaximized(!s.maximized)}>
          {s.maximized ? <Minimize2 className="size-3.5" strokeWidth={1.75} /> : <Maximize2 className="size-3.5" strokeWidth={1.75} />}
        </IconButton>
      )}
      {!s.phone && (
        <IconButton label="Close automations (Esc)" onClick={s.close}>
          <PanelRightClose className="size-4" strokeWidth={1.75} />
        </IconButton>
      )}
    </span>
  );
}

/** A page's header: Back, its icon, title and subtitle, its own tools, then Maximize and Close. */
export function SheetHeader({ icon, title, subtitle, tools }: { icon?: ReactNode; title: ReactNode; subtitle?: ReactNode; tools?: ReactNode }) {
  return (
    <header className="flex h-14 shrink-0 items-center gap-2 border-b border-line pr-2 pl-3 max-sm:h-12">
      <BackButton />
      {icon}
      <div className="min-w-0 flex-1 leading-tight">
        {typeof title === "string" ? <h2 className="truncate text-[14px] font-[590] tracking-[-0.02em]">{title}</h2> : title}
        {subtitle && <p className="truncate text-[11.5px] text-subtle">{subtitle}</p>}
      </div>
      {tools}
      <SheetTools />
    </header>
  );
}

/** A conversation a run started, in the sheet, with Back to the run (one right sheet, never two). */
function ThreadPage({ thread }: { thread: string }) {
  const { client } = useAgents();
  const s = useSheet();
  const rec = client.data.threads.get(thread);
  if (!rec)
    return (
      <>
        <SheetHeader title="Conversation" />
        <p className="p-6 text-[13px] text-muted">This conversation isn&apos;t on any of your machines right now.</p>
      </>
    );
  return (
    <Conversation
      thread={rec.value}
      machine={client.data.machines.get(rec.machine)?.value}
      start={<BackButton />}
      end={
        s &&
        !s.phone && (
          <IconButton label="Close automations (Esc)" onClick={s.close}>
            <X className="size-4" strokeWidth={1.75} />
          </IconButton>
        )
      }
    />
  );
}
