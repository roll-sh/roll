import { RollLogo } from "@roll/brand";
import { LucideProvider, Menu, X } from "lucide-react";
import { useLocation } from "@tanstack/react-router";
import { useEffect, useState, type ReactNode } from "react";
import { AgentsControl } from "./agents/agents-control";
import { AskPanel } from "./ask";
import { SignIn } from "./code/sign-in";
import { useMarkState } from "./mark-state";
import { NewProjectDialog } from "./workspace/new-project";
import { Sidebar } from "./workspace/sidebar";

/**
 * Every dashboard page's frame, Linear's inverted L on the agents surface's ladder
 * (docs/code/agents-design.md): the sidebar (projects, the month's spend) flush on the base, the
 * page an inset panel with its top bar (the crumb, the page's buttons, the agents, sign-in), and
 * the ⌘K "Ask roll" palette. `bleed` gives the page the whole panel under the top bar (the
 * project canvas) instead of the centred column.
 */
export function AppShell({ crumb, actions, bleed = false, children }: { crumb: ReactNode; actions?: ReactNode; bleed?: boolean; children: ReactNode }) {
  const { state } = useMarkState();
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [drawer, setDrawer] = useState(false);
  const pathname = useLocation({ select: (l) => l.pathname });

  useEffect(() => setDrawer(false), [pathname]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // The agents take ⌘K for their own palette while they're showing (they call preventDefault).
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k" && !e.defaultPrevented) {
        e.preventDefault();
        setPaletteOpen((o) => !o);
      }
      if (e.key === "Escape") setPaletteOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const sidebar = <Sidebar onSearch={() => setPaletteOpen(true)} />;

  return (
    <LucideProvider strokeWidth={1.5}>
      <div data-agents className={`flex w-full ${bleed ? "h-svh overflow-hidden" : "min-h-svh"}`}>
        <aside className="sticky top-0 hidden h-svh w-56 shrink-0 md:block">{sidebar}</aside>

        <div className={`flex min-w-0 flex-1 flex-col md:py-2 md:pr-2 ${bleed ? "h-svh" : ""}`}>
          <div
            className={`relative flex min-w-0 flex-1 flex-col bg-canvas md:rounded-xl md:border md:border-line-2 md:inset-shadow-highlight ${
              bleed ? "overflow-hidden" : ""
            }`}
          >
            <header className="sticky top-0 z-20 flex h-11 shrink-0 items-center gap-2 border-b border-line bg-canvas/85 px-3 backdrop-blur md:rounded-t-xl">
              <button
                onClick={() => setDrawer(true)}
                aria-label="Menu"
                className="grid size-7 place-items-center rounded-md text-muted hover:bg-white/[0.05] hover:text-fg md:hidden"
              >
                <Menu className="size-4" />
              </button>
              <span className="md:hidden">
                <RollLogo state={state} size={15} />
              </span>
              <span className="flex min-w-0 items-center gap-1.5 text-[13px] font-medium max-sm:hidden">{crumb}</span>
              <span className="ml-auto flex min-w-0 shrink items-center gap-2 overflow-x-auto [scrollbar-width:none]">
                {/* A page's buttons (Review, Land, Deploy) scroll sideways on a phone rather than widen the page. */}
                {actions}
              </span>
              <AgentsControl pill={false} />
              <SignIn />
            </header>

            {bleed ? (
              <main className="relative min-h-0 flex-1">{children}</main>
            ) : (
              <main className="mx-auto w-full max-w-6xl flex-1 px-4 py-6 md:px-6">{children}</main>
            )}
          </div>
        </div>

        {drawer && (
          <div className="fixed inset-0 z-40 md:hidden" onMouseDown={(e) => e.target === e.currentTarget && setDrawer(false)}>
            <div className="absolute inset-0 bg-[var(--scrim)]" onClick={() => setDrawer(false)} />
            <aside className="rise absolute inset-y-0 left-0 w-64 border-r border-line-2 bg-bg shadow-sheet">{sidebar}</aside>
          </div>
        )}

        <NewProjectDialog />

        {paletteOpen && (
          <div
            className="fixed inset-0 z-50 flex items-start justify-center bg-black/60 px-4 pt-[18vh] backdrop-blur-sm"
            onMouseDown={(e) => e.target === e.currentTarget && setPaletteOpen(false)}
          >
            <div className="rise relative w-full max-w-xl">
              <button onClick={() => setPaletteOpen(false)} aria-label="Close" className="absolute -top-9 right-0 text-subtle hover:text-fg">
                <X className="size-5" />
              </button>
              <AskPanel autoFocus />
            </div>
          </div>
        )}
      </div>
    </LucideProvider>
  );
}
