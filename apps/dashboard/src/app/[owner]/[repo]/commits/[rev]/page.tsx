import { GitCommitHorizontal, X } from "lucide-react";
import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { Suspense } from "react";
import { KeysTarget } from "@/components/repo/finder";
import { blobHref, changesetHref, commitsHref, param } from "@/components/repo/paths";
import { RefPicker } from "@/components/repo/ref-picker";
import { TableSkeleton } from "@/components/repo/sections";
import { firstLine, shortGit, shortId, type ChangesetSummary } from "@/lib/code-api";
import { repoMetadata } from "@/lib/code-meta";
import { getRepo, orNull, serverCode } from "@/lib/code-server";
import { requestNow } from "@/lib/now";
import { ago, formatDate } from "@/lib/time";

type Props = PageProps<"/[owner]/[repo]/commits/[rev]">;

const PAGE = 50;

export async function generateMetadata({ params, searchParams }: Props): Promise<Metadata> {
  const [{ owner, repo, rev }, q] = await Promise.all([params, searchParams]);
  const path = typeof q.path === "string" ? q.path : null;
  return repoMetadata(owner, repo, path ? `History of ${path}` : `Commits on ${param(rev)}`);
}

/** First-parent history of a ref, optionally of one path, newest first and grouped by day. */
export default function CommitsPage(props: Props) {
  return (
    <Suspense fallback={<TableSkeleton />}>
      <CommitsPageContent {...props} />
    </Suspense>
  );
}

async function CommitsPageContent({ params, searchParams }: Props) {
  const { owner, repo, rev: rawRev } = await params;
  const found = await getRepo(owner, repo);
  if (!found || !("repo" in found)) notFound();
  const r = found.repo;
  const rev = param(rawRev);
  return (
    <Suspense fallback={<TableSkeleton rows={10} />}>
      <Commits repo={r.full_name} rev={rev} query={searchParams} />
    </Suspense>
  );
}

async function Commits({ repo, rev, query }: { repo: string; rev: string; query: Props["searchParams"] }) {
  const q = await query;
  const path = typeof q.path === "string" && q.path ? q.path : undefined;
  const cursor = typeof q.cursor === "string" ? q.cursor : undefined;
  const [page, now] = await Promise.all([orNull(serverCode.log(repo, { rev, path, limit: PAGE, cursor })), requestNow()]);
  if (!page) notFound();

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
