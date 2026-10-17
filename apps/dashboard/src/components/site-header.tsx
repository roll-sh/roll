"use client";

import { RollLogo } from "@roll/brand";
import Link from "next/link";
import { useMarkState } from "./mark-state";

export function SiteHeader() {
  const { state } = useMarkState();
  return (
    <header className="sticky top-0 z-20 border-b border-line bg-bg/70 backdrop-blur">
      <div className="mx-auto flex h-14 max-w-6xl items-center gap-6 px-4">
        <Link href="/" aria-label="roll home">
          <RollLogo state={state} size={19} />
        </Link>
        <nav className="flex items-center gap-5 text-[13px] text-muted">
          <a href="#states" className="hover:text-fg">
            States
          </a>
          <a href="#profile" className="hover:text-fg">
            Profile
          </a>
          <a href="#sizes" className="hover:text-fg">
            Sizes
          </a>
          <a href="#assets" className="hover:text-fg">
            Assets
          </a>
        </nav>
        <Link
          href="/dashboard"
          className="ml-auto rounded-lg bg-fg px-3 py-1.5 text-[13px] font-medium text-bg transition hover:opacity-85"
        >
          Open dashboard
        </Link>
      </div>
    </header>
  );
}
