import { HorizonMark } from "@roll/brand";
import type { Metadata } from "next";
import Link from "next/link";
import { PublicShell } from "@/components/repo/public-header";

export const metadata: Metadata = { title: "Not found" };

/** 404 for anything roll.sh doesn't know: unknown owners, repos, paths and changesets. */
export default function NotFound() {
  return (
    <PublicShell>
      <main className="mx-auto flex w-full max-w-xl flex-col items-center px-4 py-28 text-center">
        <HorizonMark state="sleep" size={64} />
        <p className="mt-8 font-mono text-[12px] text-subtle">404</p>
        <h1 className="mt-2 text-[28px] font-semibold tracking-[-0.05em]">Nothing rolls here</h1>
        <p className="mt-3 max-w-sm text-[14px] leading-relaxed text-muted">
          This page doesn&apos;t exist, or it&apos;s private. Check the owner and repository name — or it may have been
          renamed.
        </p>
        <div className="mt-8 flex gap-2">
          <Link href="/" className="rounded-lg bg-fg px-3 py-1.5 text-[13px] font-medium text-bg transition hover:opacity-85">
            roll home
          </Link>
          <Link
            href="/dashboard/code"
            className="rounded-lg border border-line px-3 py-1.5 text-[13px] text-muted transition hover:border-line-2 hover:text-fg"
          >
            Your repositories
          </Link>
        </div>
      </main>
    </PublicShell>
  );
}
