import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { Suspense } from "react";
import { ChangeStateIcon, ChecksSummary, SourceBadge } from "@/components/code/shared";
import { changeHref } from "@/components/repo/paths";
import { TableSkeleton } from "@/components/repo/sections";
import { sortChanges, type Change } from "@/lib/code-api";
import { reviewHeadline } from "@/lib/review";
import { repoMetadata } from "@/lib/code-meta";
import { getRepo, orNull, serverCode } from "@/lib/code-server";
import { requestNow } from "@/lib/now";
import { ago } from "@/lib/time";

type Props = PageProps<"/[owner]/[repo]/changes">;

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const { owner, repo } = await params;
  return repoMetadata(owner, repo, "Changes");
}

const FILTERS = [
  { key: "open", label: "Open", match: (c: Change) => c.state === "open" || c.state === "queued" || c.state === "conflict" },
  { key: "landed", label: "Landed", match: (c: Change) => c.state === "landed" },
  { key: "closed", label: "Closed", match: (c: Change) => c.state === "closed" },
  { key: "all", label: "All", match: () => true },
] as const;

/** Changes (≈ pull requests): what agents and people have proposed, and what landed. */
export default function ChangesPage(props: Props) {
  return (
    <Suspense fallback={<TableSkeleton />}>
      <ChangesPageContent {...props} />
    </Suspense>
  );
}

async function ChangesPageContent({ params, searchParams }: Props) {
  const { owner, repo } = await params;
  const found = await getRepo(owner, repo);
  if (!found || !("repo" in found)) notFound();
  return (
    <Suspense fallback={<TableSkeleton rows={8} />}>
      <Changes repo={found.repo.full_name} query={searchParams} />
    </Suspense>
  );
}

async function Changes({ repo, query }: { repo: string; query: Props["searchParams"] }) {
  const q = await query;
  const filter = FILTERS.find((f) => f.key === q.state) ?? FILTERS[0];
  const [all, now] = await Promise.all([orNull(serverCode.changes(repo)), requestNow()]);
  const changes = all ?? [];
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
