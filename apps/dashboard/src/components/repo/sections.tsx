import { BookOpen, GitCommitHorizontal, History } from "lucide-react";
import Link from "next/link";
import { notFound } from "next/navigation";
import { firstLine, shortId, type ChangesetSummary, type Repo } from "@/lib/code-api";
import { getTree, orNull, serverCode } from "@/lib/code-server";
import { requestNow } from "@/lib/now";
import { formatCount } from "@/lib/redis-api";
import { ago } from "@/lib/time";
import { FileTable } from "./file-table";
import { KeysTarget } from "./finder";
import { changesetHref, commitsHref, treeHref } from "./paths";

const README = /^readme(\.(md|markdown|mdx|txt|rst))?$/i;

/** The latest change under a path: author, message, id and age, plus a link to the history. */
export function LastChangeBar({
  repo,
  rev,
  path,
  last,
  count,
  now,
}: {
  repo: string;
  rev: string;
  path: string;
  last: ChangesetSummary | null;
  count?: number;
  now: number;
}) {
  return (
    <div className="flex items-center gap-3 border-b border-line bg-panel-2/40 px-4 py-2.5 text-[13px]">
      {last ? (
        <>
          <span className="shrink-0 font-medium">{last.author.name}</span>
          <Link href={changesetHref(repo, last.id)} className="min-w-0 truncate text-muted hover:text-fg">
            {firstLine(last.message) || "(no message)"}
          </Link>
          <Link href={changesetHref(repo, last.id)} className="ml-auto hidden shrink-0 font-mono text-[11px] text-subtle hover:text-fg sm:inline">
            {shortId(last.id)}
          </Link>
          <span className="hidden shrink-0 text-[12px] text-subtle sm:inline">{ago(last.author.time, now)}</span>
        </>
      ) : (
        <span className="text-subtle">No history</span>
      )}
      <Link
        href={commitsHref(repo, rev, path || undefined)}
        className={`flex shrink-0 items-center gap-1.5 text-[12px] text-muted hover:text-fg ${last ? "" : "ml-auto"}`}
      >
        {count != null ? <GitCommitHorizontal className="size-3.5" /> : <History className="size-3.5" />}
        {count != null ? (
          <span>
            <span className="font-mono tabular-nums">{formatCount(count)}</span> changesets
          </span>
        ) : (
          "History"
        )}
      </Link>
    </div>
  );
}

/** A directory at a revision, with its last change on top and its README below. */
export async function Directory({ repo, rev, path }: { repo: Repo; rev: string; path: string }) {
  const full = repo.full_name;
  const [tree, log, now] = await Promise.all([
    getTree(full, rev, path),
    orNull(serverCode.log(full, { rev, path: path || undefined, limit: 1 })),
    requestNow(),
  ]);
  if (!tree) notFound();
  const readme = tree.entries.find((e) => e.kind === "file" && README.test(e.name));
  return (
    <div className="space-y-6">
      <KeysTarget rev={rev} permalink={treeHref(full, tree.changeset, path)} />
      <FileTable
        repo={full}
        rev={rev}
        tree={tree}
        now={now}
        header={
          <LastChangeBar
            repo={full}
            rev={rev}
            path={path}
            last={log?.items[0] ?? null}
            // The history behind this revision (generation counts from 0 at the root), not every
            // changeset in the repo, so it matches the commits page it links to.
            count={path || !log?.items[0] ? undefined : log.items[0].generation + 1}
            now={now}
          />
        }
      />
      {readme && <Readme repo={full} rev={tree.changeset} path={readme.path} name={readme.name} />}
    </div>
  );
}

/** A README (or any markdown file), rendered by the API and styled by `.markdown`. */
export async function Readme({ repo, rev, path, name, bare = false }: { repo: string; rev: string; path: string; name?: string; bare?: boolean }) {
  const doc = await orNull(serverCode.readme(repo, rev, path));
  if (!doc) return null;
  // The API sanitizes this HTML (see docs/code/http-api.md).
  const body = <div className="markdown px-6 py-6 sm:px-10 sm:py-8" dangerouslySetInnerHTML={{ __html: doc.html }} />;
  if (bare) return body;
  return (
    <section id="readme" className="overflow-hidden rounded-xl border border-line bg-panel">
      <div className="flex items-center gap-2 border-b border-line px-4 py-2.5 text-[13px]">
        <BookOpen className="size-3.5 text-subtle" />
        <span className="font-medium">{name ?? doc.path.split("/").pop()}</span>
      </div>
      {body}
    </section>
  );
}

export function TableSkeleton({ rows = 6 }: { rows?: number }) {
  return (
    <div className="overflow-hidden rounded-xl border border-line bg-panel">
      <div className="h-10 border-b border-line bg-panel-2/40" />
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="flex items-center gap-3 border-b border-line px-4 py-2.5 last:border-0">
          <span className="size-4 rounded bg-panel-2" />
          <span className="h-3 rounded bg-panel-2" style={{ width: `${18 + ((i * 37) % 30)}%` }} />
        </div>
      ))}
    </div>
  );
}

