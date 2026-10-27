import { Bot } from "lucide-react";
import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { Suspense } from "react";
import { SourceBadge } from "@/components/code/shared";
import { changeHref } from "@/components/repo/paths";
import { TableSkeleton } from "@/components/repo/sections";
import type { WorkItem } from "@/lib/code-api";
import { repoMetadata } from "@/lib/code-meta";
import { getRepo, orNull, serverCode } from "@/lib/code-server";
import { requestNow } from "@/lib/now";
import { ago } from "@/lib/time";

type Props = PageProps<"/[owner]/[repo]/work">;

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const { owner, repo } = await params;
  return repoMetadata(owner, repo, "Work");
}

const FILTERS = [
  { key: "active", label: "Active", match: (w: WorkItem) => w.state === "open" || w.state === "claimed" || w.state === "review" },
  { key: "done", label: "Done", match: (w: WorkItem) => w.state === "done" },
  { key: "closed", label: "Closed", match: (w: WorkItem) => w.state === "closed" },
  { key: "all", label: "All", match: () => true },
] as const;

const stateLabel: Record<WorkItem["state"], string> = {
  open: "open",
  claimed: "in progress",
  review: "in review",
  done: "done",
  closed: "closed",
};

/** Work items (≈ issues): what agents pull, who holds what, and what it became. */
export default function WorkPage(props: Props) {
  return (
    <Suspense fallback={<TableSkeleton />}>
      <WorkPageContent {...props} />
    </Suspense>
  );
}

async function WorkPageContent({ params, searchParams }: Props) {
  const { owner, repo } = await params;
  const found = await getRepo(owner, repo);
  if (!found || !("repo" in found)) notFound();
  return (
    <Suspense fallback={<TableSkeleton rows={8} />}>
      <Work repo={found.repo.full_name} query={searchParams} />
    </Suspense>
  );
}

async function Work({ repo, query }: { repo: string; query: Props["searchParams"] }) {
  const q = await query;
  const filter = FILTERS.find((f) => f.key === q.state) ?? FILTERS[0];
  const [all, now] = await Promise.all([orNull(serverCode.work(repo)), requestNow()]);
  const items = all ?? [];
  const order: Record<WorkItem["state"], number> = { claimed: 0, review: 1, open: 2, done: 3, closed: 4 };
  const shown = items
    .filter(filter.match)
    .sort((a, b) => order[a.state] - order[b.state] || a.priority - b.priority || b.updated_at - a.updated_at);

  return (
    <div className="space-y-4">
      <nav className="flex flex-wrap items-center gap-1 text-[13px]" aria-label="Filter work">
        {FILTERS.map((f) => {
          const on = f.key === filter.key;
          return (
            <Link
              key={f.key}
              href={f.key === "active" ? "?" : `?state=${f.key}`}
              className={`rounded-lg px-2.5 py-1 transition ${on ? "bg-panel-2 text-fg" : "text-muted hover:text-fg"}`}
            >
              {f.label} <span className="font-mono text-[11px] text-subtle tabular-nums">{items.filter(f.match).length}</span>
            </Link>
          );
        })}
        <p className="ml-auto text-[12px] text-subtle">
          Agents pull work with <span className="font-mono">POST …/work/claim</span>.
        </p>
      </nav>
      {shown.length === 0 ? (
        <p className="rounded-xl border border-line bg-panel px-5 py-12 text-center text-[13px] text-subtle">Nothing here.</p>
      ) : (
        <ul className="overflow-hidden rounded-xl border border-line bg-panel">
          {shown.map((w) => {
            const left = w.lease_until != null ? w.lease_until - now : null;
            return (
              <li key={w.id} className="border-b border-line last:border-0">
                <details className="group">
                  <summary className="flex cursor-pointer list-none items-start gap-3 px-4 py-3 transition hover:bg-panel-2/40">
                    <span
                      className={`mt-0.5 w-6 shrink-0 font-mono text-[11px] ${w.priority === 0 ? "text-danger" : w.priority === 1 ? "text-fg" : "text-subtle"}`}
                    >
                      P{w.priority}
                    </span>
                    <div className="min-w-0 flex-1">
                      <p className="flex flex-wrap items-center gap-2 text-[14px] font-medium">
                        <span className="min-w-0">{w.title}</span>
                        {w.labels.map((l) => (
                          <span key={l} className="rounded-full border border-line px-2 py-px text-[11px] font-normal text-muted">
                            {l}
                          </span>
                        ))}
                      </p>
                      <p className="mt-0.5 flex flex-wrap items-center gap-x-1.5 text-[12px] text-subtle">
                        <span className="font-mono">#{w.number}</span>· {stateLabel[w.state]} · opened {ago(w.created_at, now)}
                        {w.state === "claimed" && w.claimed_by && (
                          <span className="inline-flex items-center gap-1 text-muted">
                            · <Bot className="size-3.5 text-accent" /> {w.claimed_by}
                            {left != null && left > 0 && <span className="text-subtle">(lease {Math.ceil(left / 60)}m)</span>}
                          </span>
                        )}
                      </p>
                    </div>
                    {w.source !== "native" && <SourceBadge source={w.source} url={w.github_url} />}
                  </summary>
                  <div className="space-y-3 border-t border-line bg-panel-2/30 px-4 py-4 pl-13">
                    {w.body.trim() ? (
                      <p className="text-[13px] leading-relaxed whitespace-pre-wrap text-muted">{w.body}</p>
                    ) : (
                      <p className="text-[12px] text-subtle">No description.</p>
                    )}
                    {w.changes.length > 0 && (
                      <p className="flex flex-wrap gap-2 font-mono text-[12px]">
                        {w.changes.map((id) => (
                          <Link key={id} href={changeHref(repo, id)} className="text-accent hover:underline">
                            {id}
                          </Link>
                        ))}
                      </p>
                    )}
                  </div>
                </details>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
