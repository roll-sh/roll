import { notFound, permanentRedirect } from "next/navigation";
import { Suspense, type ReactNode } from "react";
import { PublicShell } from "@/components/repo/public-header";
import { RepoKeys } from "@/components/repo/finder";
import { RepoLive } from "@/components/repo/live";
import { RepoHeader } from "@/components/repo/repo-header";
import { getRepo } from "@/lib/code-server";

/** Every public repo page: the roll.sh frame, the repo header with its live strip, and the repo keys. */
export default function RepoLayout({ children, params }: LayoutProps<"/[owner]/[repo]">) {
  return (
    <Suspense fallback={<RepoFallback />}>
      <RepoFrame params={params}>{children}</RepoFrame>
    </Suspense>
  );
}

async function RepoFrame({ params, children }: { params: Promise<{ owner: string; repo: string }>; children: ReactNode }) {
  const { owner, repo } = await params;
  const found = await getRepo(owner, repo);
  if (!found) notFound();
  if ("redirect" in found) permanentRedirect(`/${found.redirect}`);
  const r = found.repo;
  return (
    <PublicShell>
      <RepoLive
        repo={r.full_name}
        mergesToday={r.stats.merges_today}
        rate={r.stats.merges_per_s}
        queueDepth={r.land_policy === "post_merge" ? null : r.stats.queue_depth}
        queueBatch={r.stats.queue_batch ?? 0}
      >
        <RepoKeys repo={r.full_name} defaultRev={r.default_branch}>
          <RepoHeader repo={r} />
          <main className="mx-auto w-full max-w-6xl px-4 py-8">{children}</main>
        </RepoKeys>
      </RepoLive>
    </PublicShell>
  );
}

function RepoFallback() {
  return (
    <PublicShell>
      <div className="border-b border-line">
        <div className="mx-auto max-w-6xl px-4 pt-8 pb-14">
          <div className="h-6 w-64 rounded-md bg-panel-2" />
          <div className="mt-3 h-4 w-96 max-w-full rounded-md bg-panel-2/70" />
        </div>
      </div>
      <main className="mx-auto w-full max-w-6xl px-4 py-8">
        <p className="text-[13px] text-subtle">
          <span className="shimmer">Loading repository…</span>
        </p>
      </main>
    </PublicShell>
  );
}
