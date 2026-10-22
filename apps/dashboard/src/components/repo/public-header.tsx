"use client";

import { HorizonMark, RollLogo } from "@roll/brand";
import { LucideProvider } from "lucide-react";
import Link from "next/link";
import type { ReactNode } from "react";
import { useMarkState } from "../mark-state";

/** The roll.sh frame for public pages: Horizon logo, an optional crumb, and a quiet footer. */
export function PublicShell({ crumb, children }: { crumb?: ReactNode; children: ReactNode }) {
  const { state } = useMarkState();
  return (
    <LucideProvider strokeWidth={1.5}>
      <header className="sticky top-0 z-30 border-b border-line bg-bg/80 backdrop-blur">
        <div className="mx-auto flex h-14 max-w-6xl items-center gap-3 px-4">
          <Link href="/" aria-label="roll home" className="shrink-0">
            <RollLogo state={state} size={19} />
          </Link>
          {crumb && (
            <>
              <span className="text-line-2">/</span>
              <div className="min-w-0 truncate text-[13px] text-muted">{crumb}</div>
            </>
          )}
          <Link
            href="/dashboard/code"
            className="ml-auto shrink-0 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-muted transition hover:border-line-2 hover:text-fg"
          >
            Dashboard
          </Link>
        </div>
      </header>
      <div className="flex-1">{children}</div>
      <footer className="mt-24 border-t border-line">
        <div className="mx-auto flex h-14 max-w-6xl items-center gap-2 px-4 text-[12px] text-subtle">
          <HorizonMark size={14} interactive={false} />
          <span>keep the code rolling</span>
          <span className="ml-auto font-mono text-[11px]">roll.sh</span>
        </div>
      </footer>
    </LucideProvider>
  );
}
