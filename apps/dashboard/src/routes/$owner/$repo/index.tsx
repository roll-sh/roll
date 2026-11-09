import { createFileRoute } from "@tanstack/react-router";
import { createServerFn } from "@tanstack/react-start";
import { LatestLandings } from "@/components/repo/live";
import { Languages } from "@/components/repo/meta";
import { QuickStart } from "@/components/repo/quick-start";
import { RefPicker } from "@/components/repo/ref-picker";
import { Directory, TableSkeleton } from "@/components/repo/sections";
import { codeApi, type ChangesetSummary, type Repo } from "@/lib/code-api";
import { repoHead } from "@/lib/code-meta";
import { orNull, readDirectory, serverCode } from "@/lib/code-server";
import { or404 } from "@/lib/public-pages";
import { formatCount } from "@/lib/redis-api";
import { formatDate } from "@/lib/time";

const loadHome = createServerFn({ method: "GET" })
  .validator((d: { repo: string; rev: string }) => d)
  .handler(async ({ data: { repo, rev } }) => {
    const [dir, log] = await Promise.all([readDirectory(repo, rev, ""), orNull(serverCode().log(repo, { rev, limit: 8 }))]);
    return { dir, landings: log?.items ?? [] };
  });

export const Route = createFileRoute("/$owner/$repo/")({
  loader: async ({ context: { repo } }) => {
    if (!repo.head) return null;
    const { dir, landings } = await loadHome({ data: { repo: repo.full_name, rev: repo.default_branch } });
    return { dir: or404(dir), landings };
  },
  head: ({ match }) => repoHead(match.context.repo),
  component: RepoHome,
  pendingComponent: () => <TableSkeleton />,
});

/** A repo's home: files and README on the left; about, languages, stats and live landings on the right. */
function RepoHome() {
  const { repo: r } = Route.useRouteContext();
  const home = Route.useLoaderData();

  if (!home) {
    return (
      <div className="mx-auto max-w-3xl">
        <QuickStart repo={r} apiUrl={codeApi.url(r.full_name)} />
      </div>
    );
  }

  return (
    <div className="grid gap-10 lg:grid-cols-[minmax(0,1fr)_17rem]">
      <div className="min-w-0 space-y-4">
        <div className="flex items-center gap-3">
          <RefPicker repo={r.full_name} rev={r.default_branch} kind="tree" />
        </div>
        <Directory repo={r} rev={r.default_branch} path="" data={home.dir} />
      </div>
      <Rail repo={r} landings={home.landings} now={home.dir.now} />
    </div>
  );
}

function Rail({ repo, landings, now }: { repo: Repo; landings: ChangesetSummary[]; now: number }) {
  const s = repo.stats;
  const stats = [
    ["Merges today", formatCount(s.merges_today)],
    ["Changesets", formatCount(s.changesets)],
    ["Contributors", formatCount(s.contributors)],
    ["Open changes", formatCount(s.open_changes)],
    ["Open work", formatCount(s.open_work)],
  ] as const;
  return (
    <aside className="min-w-0 space-y-9 text-[13px]">
      <section>
        <h2 className="text-[13px] font-semibold tracking-[-0.01em]">About</h2>
        <p className="mt-2 leading-relaxed text-muted">{repo.description ?? <span className="text-subtle">No description.</span>}</p>
        <p className="mt-3 font-mono text-[11px] text-subtle">
          {repo.source === "github" ? "mirrored to GitHub" : "native on roll"} · since {formatDate(repo.created_at)}
        </p>
      </section>
      {Object.keys(repo.languages).length > 0 && (
        <section>
          <h2 className="mb-3 text-[13px] font-semibold tracking-[-0.01em]">Languages</h2>
          <Languages languages={repo.languages} />
        </section>
      )}
      <section>
        <h2 className="text-[13px] font-semibold tracking-[-0.01em]">Activity</h2>
        <dl className="mt-2 divide-y divide-line">
          {stats.map(([k, v]) => (
            <div key={k} className="flex items-baseline justify-between py-1.5">
              <dt className="text-muted">{k}</dt>
              <dd className="font-mono text-[12px] tabular-nums">{v}</dd>
            </div>
          ))}
        </dl>
      </section>
      <section>
        <h2 className="mb-3 text-[13px] font-semibold tracking-[-0.01em]">Latest landings</h2>
        <LatestLandings repo={repo.full_name} initial={landings} now={now} />
      </section>
    </aside>
  );
}
