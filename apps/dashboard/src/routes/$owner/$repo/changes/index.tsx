import { createFileRoute } from "@tanstack/react-router";
import { Link } from "@/components/link";
import { createServerFn } from "@tanstack/react-start";
import { ChangeStateIcon, ChecksSummary, SourceBadge } from "@/components/code/shared";
import { changeHref } from "@/components/repo/paths";
import { TableSkeleton } from "@/components/repo/sections";
import { sortChanges, type Change } from "@/lib/code-api";
import { repoHead } from "@/lib/code-meta";
import { nowSeconds, orNull, serverCode } from "@/lib/code-server";
import { reviewHeadline } from "@/lib/review";
import { ago } from "@/lib/time";

const FILTERS = [
  { key: "open", label: "Open", match: (c: Change) => c.state === "open" || c.state === "queued" || c.state === "conflict" },
  { key: "landed", label: "Landed", match: (c: Change) => c.state === "landed" },
  { key: "closed", label: "Closed", match: (c: Change) => c.state === "closed" },
  { key: "all", label: "All", match: () => true },
] as const;

const loadChanges = createServerFn({ method: "GET" })
  .validator((repo: string) => repo)
  .handler(async ({ data: repo }) => ({ changes: (await orNull(serverCode().changes(repo))) ?? [], now: nowSeconds() }));

/** Changes (≈ pull requests): what agents and people have proposed, and what landed. */
export const Route = createFileRoute("/$owner/$repo/changes/")({
  loader: ({ context: { repo } }) => loadChanges({ data: repo.full_name }),
  head: ({ match }) => repoHead(match.context.repo, "Changes"),
  component: Changes,
  pendingComponent: () => <TableSkeleton rows={8} />,
});

function Changes() {
  const repo = Route.useRouteContext().repo.full_name;
  const { state } = Route.useSearch();
  const { changes, now } = Route.useLoaderData();
  const filter = FILTERS.find((f) => f.key === state) ?? FILTERS[0];
  const shown = sortChanges(changes.filter(filter.match));
  return (
    <div className="space-y-4">
      <nav className="flex flex-wrap items-center gap-1 text-[13px]" aria-label="Filter changes">
        {FILTERS.map((f) => {
          const n = changes.filter(f.match).length;
          const on = f.key === filter.key;
          return (
            <Link
              key={f.key}
              href={f.key === "open" ? "?" : `?state=${f.key}`}
              className={`rounded-lg px-2.5 py-1 transition ${on ? "bg-panel-2 text-fg" : "text-muted hover:text-fg"}`}
            >
              {f.label} <span className="font-mono text-[11px] text-subtle tabular-nums">{n}</span>
            </Link>
          );
        })}
        <p className="ml-auto text-[12px] text-subtle">
          Open one by pushing a branch, or <span className="font-mono">POST …/changes</span>.
        </p>
      </nav>
      {shown.length === 0 ? (
        <p className="rounded-xl border border-line bg-panel px-5 py-12 text-center text-[13px] text-subtle">
          {filter.key === "open" ? "Nothing in flight — every change has landed or closed." : `No ${filter.label.toLowerCase()} changes.`}
        </p>
      ) : (
        <ul className="overflow-hidden rounded-xl border border-line bg-panel">
          {shown.map((c) => (
            <li key={c.id} className="border-b border-line last:border-0">
              <Link href={changeHref(repo, c.number)} className="flex items-start gap-3 px-4 py-3 transition hover:bg-panel-2/40">
                <span className="mt-0.5">
                  <ChangeStateIcon state={c.state} />
                </span>
                <div className="min-w-0 flex-1">
                  <p className="flex items-center gap-2 text-[14px] font-medium">
                    <span className="truncate">{c.title}</span>
                    {c.github_pr != null && <SourceBadge source={`github#${c.github_pr}`} url={null} />}
                  </p>
                  <p className="mt-0.5 truncate text-[12px] text-subtle">
                    <span className="font-mono">#{c.number}</span> · {c.author} · {c.state === "landed" ? "landed" : "updated"}{" "}
                    {ago(c.updated_at, now)}
                    {c.revision > 1 && ` · rev ${c.revision}`}
                    {c.conflict && <span className="text-danger"> · {c.conflict.paths.length} conflicting</span>}
                    {c.review && c.state !== "landed" && c.state !== "closed" && c.review.state !== "pending" && (
                      <span className={c.review.state === "changes_requested" ? "text-danger" : "text-ok"}>
                        {" "}
                        · {reviewHeadline(c.review).label.toLowerCase()}
                      </span>
                    )}
                  </p>
                </div>
                <span className="hidden shrink-0 pt-1 sm:block">
                  <ChecksSummary checks={c.checks} />
                </span>
                {c.preview?.state === "running" && (
                  <span className="mt-2 inline-block size-1.5 shrink-0 rounded-full bg-ok" title="Preview running" />
                )}
              </Link>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
