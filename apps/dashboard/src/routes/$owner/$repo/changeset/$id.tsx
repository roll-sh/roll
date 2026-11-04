import { createFileRoute } from "@tanstack/react-router";
import { Link } from "@/components/link";
import { createServerFn } from "@tanstack/react-start";
import { DiffViewer } from "@/components/code/diff";
import { CopyLine } from "@/components/repo/clone-box";
import { KeysTarget } from "@/components/repo/finder";
import { changesetHref, treeHref } from "@/components/repo/paths";
import { TableSkeleton } from "@/components/repo/sections";
import { firstLine, shortGit, shortId } from "@/lib/code-api";
import { repoHead } from "@/lib/code-meta";
import { nowSeconds, orNull, serverCode } from "@/lib/code-server";
import { or404 } from "@/lib/public-pages";
import { ago, formatDateTime } from "@/lib/time";

const loadChangeset = createServerFn({ method: "GET" })
  .validator((d: { repo: string; id: string }) => d)
  .handler(async ({ data: { repo, id } }) => ({ cs: await orNull(serverCode().changeset(repo, id)), now: nowSeconds() }));

/** One changeset: message, who and when, ids, parents and the diff against its first parent. */
export const Route = createFileRoute("/$owner/$repo/changeset/$id")({
  loader: async ({ context: { repo }, params: { id } }) => {
    const { cs, now } = await loadChangeset({ data: { repo: repo.full_name, id } });
    return { cs: or404(cs), now };
  },
  head: ({ match, loaderData, params: { id } }) => {
    const cs = loaderData?.cs;
    return repoHead(match.context.repo, cs ? `${firstLine(cs.message) || shortId(cs.id)} · ${shortId(cs.id)}` : shortId(id));
  },
  component: ChangesetView,
  pendingComponent: () => <TableSkeleton />,
});

function ChangesetView() {
  const repo = Route.useRouteContext().repo.full_name;
  const { cs, now } = Route.useLoaderData();
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
