import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { Suspense } from "react";
import { LatestLandings } from "@/components/repo/live";
import { Languages } from "@/components/repo/meta";
import { RefPicker } from "@/components/repo/ref-picker";
import { QuickStart } from "@/components/repo/quick-start";
import { Directory, TableSkeleton } from "@/components/repo/sections";
import { codeApi, type Repo } from "@/lib/code-api";
import { repoMetadata } from "@/lib/code-meta";
import { getRepo, orNull, serverCode } from "@/lib/code-server";
import { requestNow } from "@/lib/now";
import { formatCount } from "@/lib/redis-api";
import { formatDate } from "@/lib/time";

export async function generateMetadata({ params }: PageProps<"/[owner]/[repo]">): Promise<Metadata> {
  const { owner, repo } = await params;
  return repoMetadata(owner, repo);
}

/** A repo's home: files and README on the left; about, languages, stats and live landings on the right. */
export default function RepoHome(props: PageProps<"/[owner]/[repo]">) {
  return (
    <Suspense fallback={<TableSkeleton />}>
      <RepoHomeContent {...props} />
    </Suspense>
  );
}

async function RepoHomeContent({ params }: PageProps<"/[owner]/[repo]">) {
  const { owner, repo } = await params;
  const found = await getRepo(owner, repo);
  if (!found || !("repo" in found)) notFound();
  const r = found.repo;

  if (!r.head) {
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
        <Suspense fallback={<TableSkeleton />}>
          <Directory repo={r} rev={r.default_branch} path="" />
        </Suspense>
      </div>
      <Rail repo={r} />
    </div>
  );
}

function Rail({ repo }: { repo: Repo }) {
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
        <Suspense fallback={<p className="text-[12px] text-subtle">Loading…</p>}>
          <Landings repo={repo} />
        </Suspense>
      </section>
    </aside>
  );
}

async function Landings({ repo }: { repo: Repo }) {
  const [log, now] = await Promise.all([orNull(serverCode.log(repo.full_name, { rev: repo.default_branch, limit: 8 })), requestNow()]);
  return <LatestLandings repo={repo.full_name} initial={log?.items ?? []} now={now} />;
}
