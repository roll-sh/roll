import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { Suspense } from "react";
import { DiffViewer } from "@/components/code/diff";
import { KeysTarget } from "@/components/repo/finder";
import { changesetHref, param, treeHref } from "@/components/repo/paths";
import { CopyLine } from "@/components/repo/clone-box";
import { TableSkeleton } from "@/components/repo/sections";
import { firstLine, shortGit, shortId } from "@/lib/code-api";
import { repoMetadata } from "@/lib/code-meta";
import { getRepo, orNull, serverCode } from "@/lib/code-server";
import { requestNow } from "@/lib/now";
import { ago, formatDateTime } from "@/lib/time";

type Props = PageProps<"/[owner]/[repo]/changeset/[id]">;

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const { owner, repo, id } = await params;
  const cs = await orNull(serverCode.changeset(`${owner}/${repo}`, param(id)));
  return repoMetadata(owner, repo, cs ? `${firstLine(cs.message) || shortId(cs.id)} · ${shortId(cs.id)}` : shortId(param(id)));
}

/** One changeset: message, who and when, ids, parents and the diff against its first parent. */
export default function ChangesetPage(props: Props) {
  return (
    <Suspense fallback={<TableSkeleton />}>
      <ChangesetPageContent {...props} />
    </Suspense>
  );
}

async function ChangesetPageContent({ params }: Props) {
  const { owner, repo, id } = await params;
  const found = await getRepo(owner, repo);
  if (!found || !("repo" in found)) notFound();
  return (
    <Suspense fallback={<TableSkeleton />}>
      <ChangesetView repo={found.repo.full_name} id={param(id)} />
    </Suspense>
  );
}

async function ChangesetView({ repo, id }: { repo: string; id: string }) {
  const [cs, now] = await Promise.all([orNull(serverCode.changeset(repo, id)), requestNow()]);
  if (!cs) notFound();
  const title = firstLine(cs.message) || "(no message)";
  const body = cs.message.slice(cs.message.indexOf("\n") + 1).trim();
  const hasBody = cs.message.includes("\n") && body.length > 0;
  const sameCommitter = cs.committer.name === cs.author.name && cs.committer.email === cs.author.email;
  return (
    <div className="space-y-8">
      <KeysTarget rev={cs.id} permalink={changesetHref(repo, cs.id)} />
      <header className="overflow-hidden rounded-xl border border-line bg-panel">
        <div className="px-6 py-5">
          <h1 className="text-[20px] leading-snug font-semibold tracking-[-0.035em]">{title}</h1>
          {hasBody && <pre className="mt-3 font-sans text-[13px] leading-relaxed whitespace-pre-wrap text-muted">{body}</pre>}
        </div>
        <div className="flex flex-wrap items-center gap-x-6 gap-y-3 border-t border-line bg-panel-2/40 px-6 py-3 text-[12px]">
          <p className="text-muted">
            <span className="font-medium text-fg">{cs.author.name}</span> authored{" "}
            <span title={formatDateTime(cs.author.time)}>{ago(cs.author.time, now)}</span>
            {!sameCommitter && (
              <>
                {" · "}
                <span className="text-fg">{cs.committer.name}</span> landed {ago(cs.committer.time, now)}
              </>
            )}
          </p>
          <p className="font-mono text-[11px] text-subtle">
            {cs.parents.length === 0 ? (
              "root"
            ) : (
              <>
                {cs.parents.length > 1 ? "parents " : "parent "}
                {cs.parents.map((p, i) => (
                  <span key={p}>
                    {i > 0 && " + "}
                    <Link href={changesetHref(repo, p)} className="text-muted hover:text-fg">
                      {shortId(p)}
                    </Link>
                  </span>
                ))}
              </>
            )}
            {" · "}gen {cs.generation.toLocaleString("en-US")}
            {cs.git_commit && <> · git {shortGit(cs.git_commit)}</>}
          </p>
          <div className="ml-auto flex items-center gap-2">
            <CopyLine text={cs.id} className="w-56" />
            <Link
              href={treeHref(repo, cs.id)}
              className="rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-muted transition hover:border-line-2 hover:text-fg"
            >
              Browse files
            </Link>
          </div>
        </div>
      </header>
      <DiffViewer files={cs.files} />
    </div>
  );
}
