import { createFileRoute } from "@tanstack/react-router";
import { createServerFn } from "@tanstack/react-start";
import { GitCommitHorizontal, X } from "lucide-react";
import { Link } from "@/components/link";
import { KeysTarget } from "@/components/repo/finder";
import { blobHref, changesetHref, commitsHref } from "@/components/repo/paths";
import { RefPicker } from "@/components/repo/ref-picker";
import { TableSkeleton } from "@/components/repo/sections";
import { firstLine, shortGit, shortId, type ChangesetSummary } from "@/lib/code-api";
import { repoHead } from "@/lib/code-meta";
import { nowSeconds, orNull, serverCode } from "@/lib/code-server";
import { or404 } from "@/lib/public-pages";
import { ago, formatDate } from "@/lib/time";

const PAGE = 50;

const loadCommits = createServerFn({ method: "GET" })
  .validator((d: { repo: string; rev: string; path?: string; cursor?: string }) => d)
  .handler(async ({ data: { repo, rev, path, cursor } }) => ({
    page: await orNull(serverCode().log(repo, { rev, path, limit: PAGE, cursor })),
    now: nowSeconds(),
  }));

/** First-parent history of a ref, optionally of one path, newest first and grouped by day. */
export const Route = createFileRoute("/$owner/$repo/commits/$rev")({
  loaderDeps: ({ search }) => ({ path: search.path || undefined, cursor: search.cursor }),
  loader: async ({ context: { repo }, params: { rev }, deps }) => {
    const { page, now } = await loadCommits({ data: { repo: repo.full_name, rev, ...deps } });
    return { page: or404(page), now };
  },
  head: ({ match, params: { rev } }) => {
    const path = match.search.path;
    return repoHead(match.context.repo, path ? `History of ${path}` : `Commits on ${rev}`);
  },
  component: Commits,
  pendingComponent: () => <TableSkeleton rows={10} />,
});

function Commits() {
  const repo = Route.useRouteContext().repo.full_name;
  const { rev } = Route.useParams();
  const { path, cursor } = Route.useLoaderDeps();
  const { page, now } = Route.useLoaderData();

  const days: [string, ChangesetSummary[]][] = [];
  for (const c of page.items) {
    const day = formatDate(c.author.time);
    if (days.at(-1)?.[0] === day) days.at(-1)![1].push(c);
    else days.push([day, [c]]);
  }
  const more = (c: string) => `${commitsHref(repo, rev, path)}${path ? "&" : "?"}cursor=${encodeURIComponent(c)}`;

  return (
    <div className="space-y-6">
      <KeysTarget rev={rev} permalink={page.items[0] ? commitsHref(repo, page.items[0].id, path) : null} />
      <div className="flex flex-wrap items-center gap-3">
        <RefPicker repo={repo} rev={rev} kind="commits" path={path} />
        {path ? (
          <p className="flex min-w-0 items-center gap-2 text-[13px] text-muted">
            History of{" "}
            <Link href={blobHref(repo, rev, path)} className="truncate font-mono text-[12px] text-accent hover:underline">
              {path}
            </Link>
            <Link href={commitsHref(repo, rev)} className="text-subtle hover:text-fg" aria-label="Show all history">
              <X className="size-3.5" />
            </Link>
          </p>
        ) : (
          <p className="text-[13px] text-muted">First-parent history — what landed, in order.</p>
        )}
      </div>

      {days.length === 0 ? (
        <p className="rounded-xl border border-line bg-panel px-5 py-8 text-center text-[13px] text-subtle">No changesets.</p>
      ) : (
        days.map(([day, items]) => (
          <section key={day}>
            <h2 className="mb-2 flex items-center gap-2 text-[12px] text-subtle">
              <GitCommitHorizontal className="size-3.5" /> {day}
            </h2>
            <ul className="overflow-hidden rounded-xl border border-line bg-panel">
              {items.map((c) => (
                <li key={c.id} className="flex items-center gap-4 border-b border-line px-4 py-3 last:border-0 hover:bg-panel-2/40">
                  <div className="min-w-0 flex-1">
                    <Link href={changesetHref(repo, c.id)} className="block truncate text-[13px] font-medium hover:text-accent">
                      {firstLine(c.message) || "(no message)"}
                    </Link>
                    <p className="mt-0.5 truncate text-[12px] text-subtle">
                      <span className="text-muted">{c.author.name}</span> landed {ago(c.author.time, now)}
                      {c.parents.length > 1 && " · merge"} · {c.files_changed} file{c.files_changed === 1 ? "" : "s"}
                    </p>
                  </div>
                  <div className="hidden shrink-0 text-right font-mono text-[11px] sm:block">
                    <Link href={changesetHref(repo, c.id)} className="block text-muted hover:text-fg">
                      {shortId(c.id)}
                    </Link>
                    {c.git_commit && <span className="text-subtle">git {shortGit(c.git_commit)}</span>}
                  </div>
                </li>
              ))}
            </ul>
          </section>
        ))
      )}

      <div className="flex justify-center gap-2">
        {cursor && (
          <Link href={commitsHref(repo, rev, path)} className="rounded-lg border border-line px-3 py-1.5 text-[12px] text-muted hover:border-line-2 hover:text-fg">
            Newest
          </Link>
        )}
        {page.cursor && (
          <Link href={more(page.cursor)} className="rounded-lg border border-line px-3 py-1.5 text-[12px] text-muted hover:border-line-2 hover:text-fg">
            Older →
          </Link>
        )}
      </div>
    </div>
  );
}
