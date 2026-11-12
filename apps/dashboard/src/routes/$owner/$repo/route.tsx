import { createFileRoute, Outlet } from "@tanstack/react-router";
import { RepoKeys } from "@/components/repo/finder";
import { RepoLive } from "@/components/repo/live";
import { PublicShell } from "@/components/repo/public-header";
import { RepoHeader } from "@/components/repo/repo-header";
import { lookupRepo } from "@/lib/code-server";
import { ownerOr404, repoOr404 } from "@/lib/public-pages";

/**
 * Every public repo page: the roll.sh frame, the repo header with its live strip, and the repo
 * keys. The repo is looked up before any page under it loads, so an unknown repo is a 404 and a
 * renamed one a 308; the pages read it from the route context.
 */
export const Route = createFileRoute("/$owner/$repo")({
  beforeLoad: async ({ params, location }) => {
    ownerOr404(params.owner);
    const found = await lookupRepo({ data: { owner: params.owner, repo: params.repo } });
    return { repo: repoOr404(found, location.pathname, location.searchStr) };
  },
  component: RepoFrame,
  pendingComponent: RepoFallback,
});

function RepoFrame() {
  const { repo: r } = Route.useRouteContext();
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
          <main className="mx-auto w-full max-w-6xl px-4 py-8">
            <Outlet />
          </main>
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
