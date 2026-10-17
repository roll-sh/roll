"use client";

import { RollLogo } from "@roll/brand";
import {
  Activity,
  ChevronsUpDown,
  Database,
  Globe,
  LayoutGrid,
  Rocket,
  ScrollText,
  Search,
  Settings,
  SquareFunction,
  Workflow,
  X,
} from "lucide-react";
import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useState, type ReactNode } from "react";
import { AskPanel } from "./ask";
import { useMarkState } from "./mark-state";

const nav = [
  { label: "Overview", icon: LayoutGrid, href: "/dashboard" },
  { label: "Databases", icon: Database, href: "/dashboard/databases" },
  { label: "Tasks", icon: Workflow, href: "/dashboard/tasks" },
  { label: "Functions", icon: SquareFunction },
  { label: "Deployments", icon: Rocket },
  { label: "Edge", icon: Globe },
  { label: "Logs", icon: ScrollText },
  { label: "Observability", icon: Activity },
  { label: "Settings", icon: Settings },
];

/** Sidebar, top bar and the ⌘K "Ask roll" palette shared by every dashboard page. */
export function AppShell({ crumb, actions, children }: { crumb: string; actions?: ReactNode; children: ReactNode }) {
  const { state } = useMarkState();
  const pathname = usePathname();
  const [paletteOpen, setPaletteOpen] = useState(false);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPaletteOpen((o) => !o);
      }
      if (e.key === "Escape") setPaletteOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  return (
    <div className="flex min-h-svh">
      <aside className="sticky top-0 hidden h-svh w-60 shrink-0 flex-col border-r border-line bg-panel px-3 py-4 md:flex">
        <Link href="/" className="px-2 py-1" aria-label="roll home">
          <RollLogo state={state} size={19} />
        </Link>
        <button className="mt-5 flex items-center gap-2 rounded-lg border border-line px-2.5 py-2 text-left text-[13px] hover:border-line-2">
          <span className="grid size-5 place-items-center rounded bg-accent text-[10px] font-bold text-white">A</span>
          <span className="flex-1">acme</span>
          <ChevronsUpDown className="size-3.5 text-subtle" />
        </button>
        <nav className="mt-5 space-y-0.5">
          {nav.map(({ label, icon: Icon, href }) => {
            const active = href === pathname;
            const cls = `flex items-center gap-2.5 rounded-lg px-2.5 py-1.5 text-[13px] transition ${
              active ? "bg-panel-2 text-fg" : "text-muted hover:bg-panel-2 hover:text-fg"
            }`;
            const inner = (
              <>
                <Icon className="size-4" strokeWidth={1.75} />
                {label}
              </>
            );
            return href ? (
              <Link key={label} href={href} className={cls}>
                {inner}
              </Link>
            ) : (
              <a key={label} href="#" className={cls}>
                {inner}
              </a>
            );
          })}
        </nav>
        <p className="mt-auto px-2 text-[11px] text-subtle">Usage · 41% of Pro plan</p>
      </aside>

      <div className="flex min-w-0 flex-1 flex-col">
        <header className="sticky top-0 z-10 flex h-14 items-center gap-3 border-b border-line bg-bg/80 px-4 backdrop-blur">
          <span className="md:hidden">
            <RollLogo state={state} size={17} />
          </span>
          <span className="hidden text-[13px] text-muted sm:inline">
            acme <span className="text-subtle">/</span> <span className="text-fg">{crumb}</span>
          </span>
          <button
            onClick={() => setPaletteOpen(true)}
            className="ml-auto flex w-full max-w-xs items-center gap-2 rounded-lg border border-line bg-panel px-3 py-1.5 text-[13px] text-subtle hover:border-line-2"
          >
            <Search className="size-3.5" />
            <span className="flex-1 text-left">Ask roll…</span>
            <kbd className="rounded border border-line px-1.5 font-mono text-[10px]">⌘K</kbd>
          </button>
          {actions}
        </header>

        <main className="mx-auto w-full max-w-6xl flex-1 px-4 py-8">{children}</main>
      </div>

      {paletteOpen && (
        <div
          className="fixed inset-0 z-50 flex items-start justify-center bg-black/60 px-4 pt-[18vh] backdrop-blur-sm"
          onMouseDown={(e) => e.target === e.currentTarget && setPaletteOpen(false)}
        >
          <div className="rise relative w-full max-w-xl">
            <button
              onClick={() => setPaletteOpen(false)}
              aria-label="Close"
              className="absolute -top-9 right-0 text-subtle hover:text-fg"
            >
              <X className="size-5" />
            </button>
            <AskPanel autoFocus />
          </div>
        </div>
      )}
    </div>
  );
}
