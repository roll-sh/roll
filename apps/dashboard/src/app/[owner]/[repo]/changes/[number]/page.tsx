import { Database, ExternalLink, GitMergeConflict, MemoryStick } from "lucide-react";
import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { Suspense } from "react";
import { DiffViewer } from "@/components/code/diff";
import { SearchForm } from "@/components/code/search";
import { ReviewStatusView, ReviewTimeline } from "@/components/code/review";
import { BlockerList, ChangeStateBadge, CheckIcon, ChecksSummary, GitHubMark, QueuePill } from "@/components/code/shared";
import { KeysTarget } from "@/components/repo/finder";
import { changeHref, changesetHref, param } from "@/components/repo/paths";
import { TableSkeleton } from "@/components/repo/sections";
import { shortId, type Change, type ReviewStatus } from "@/lib/code-api";
import { checkRows, type CheckRow } from "@/lib/ci";
import { repoMetadata } from "@/lib/code-meta";
import { getRepo, orNull, serverCode } from "@/lib/code-server";
import { requestNow } from "@/lib/now";
import { ago } from "@/lib/time";

type Props = PageProps<"/[owner]/[repo]/changes/[number]">;

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const { owner, repo, number } = await params;
  const c = await orNull(serverCode.change(`${owner}/${repo}`, param(number)));
  return repoMetadata(owner, repo, c ? `${c.title} · #${c.number}` : `Change ${param(number)}`);
}

/**
 * One change, read-only: description, the diff with its review threads, reviews, checks and
 * preview. Reviewing and landing happen through the API or the dashboard.
 */
export default function ChangePage(props: Props) {
  return (
    <Suspense fallback={<TableSkeleton />}>
      <ChangePageContent {...props} />
    </Suspense>
  );
}

async function ChangePageContent({ params }: Props) {
  const { owner, repo, number } = await params;
  const found = await getRepo(owner, repo);
  if (!found || !("repo" in found)) notFound();
  return (
    <Suspense fallback={<TableSkeleton />}>
      <ChangeView repo={found.repo.full_name} number={param(number)} />
    </Suspense>
  );
}

async function ChangeView({ repo, number }: { repo: string; number: string }) {
  const [c, now] = await Promise.all([orNull(serverCode.change(repo, number)), requestNow()]);
  if (!c) notFound();
  // Reviews are extra: a server without them (or a hiccup) still shows the change.
  const soft = <T,>(p: Promise<T>) => p.catch(() => null);
  const [diff, reviews, threads, status, statuses] = await Promise.all([
    orNull(serverCode.compare(repo, c.base, c.head)),
    soft(serverCode.reviews(c.id)),
    soft(serverCode.threads(c.id)),
    soft(serverCode.reviewStatus(c.id)),
    soft(serverCode.statuses(repo, c.head)),
  ]);
  const checks = checkRows(c.checks, statuses?.statuses);
  return (
    <div className="space-y-8">
      <KeysTarget rev={c.head} permalink={changeHref(repo, c.number)} />
      <header>
        <h1 className="text-[24px] leading-tight font-semibold tracking-[-0.04em]">
          {c.title} <span className="font-normal text-subtle">#{c.number}</span>
        </h1>
        <div className="mt-3 flex flex-wrap items-center gap-x-2 gap-y-1 text-[13px] text-muted">
          <ChangeStateBadge state={c.state} />
          {c.queue && (c.state === "queued" || c.queue.state === "ejected") && <QueuePill queue={c.queue} />}
          {c.draft && <span className="rounded-full border border-line px-2 py-0.5 text-[12px] text-subtle">draft</span>}
          <span className="text-fg">{c.author}</span>
          <span>
            opened {ago(c.created_at, now)}
            {c.revision > 1 && ` · revision ${c.revision}`}
          </span>
          <span className="font-mono text-[11px] text-subtle">
            {shortId(c.base)} → {shortId(c.head)}
          </span>
          {c.github_url && (
            <a href={c.github_url} target="_blank" rel="noreferrer" className="inline-flex items-center gap-1 text-[12px] hover:text-fg">
              <GitHubMark className="size-3" /> #{c.github_pr}
            </a>
          )}
        </div>
      </header>

      {c.state === "landed" && c.landed_as && (
        <p className="rounded-lg border border-line bg-panel px-4 py-3 text-[13px] text-muted">
          Landed as{" "}
          <Link href={changesetHref(repo, c.landed_as)} className="font-mono text-fg hover:text-accent">
            {shortId(c.landed_as)}
          </Link>
          .
        </p>
      )}
      {c.conflict && c.state !== "landed" && (
        <div className="rounded-lg border border-danger/40 bg-danger/5 px-4 py-3 text-[13px]">
          <p className="flex items-center gap-2 text-danger">
            <GitMergeConflict className="size-4" /> {c.conflict.paths.length} file{c.conflict.paths.length === 1 ? "" : "s"} changed
            on the default branch since this change&apos;s base and couldn&apos;t merge cleanly.
          </p>
          <ul className="mt-2 space-y-0.5 font-mono text-[12px] text-muted">
            {c.conflict.paths.map((p) => (
              <li key={p}>{p}</li>
            ))}
          </ul>
        </div>
      )}

      <div className="grid gap-10 lg:grid-cols-[minmax(0,1fr)_17rem]">
        <div className="min-w-0 space-y-8">
          {c.body.trim() && (
            <section className="rounded-xl border border-line bg-panel px-6 py-5">
              <p className="text-[14px] leading-relaxed whitespace-pre-wrap text-muted">{c.body}</p>
            </section>
          )}
          {diff ? (
            <DiffViewer files={diff.files} review={threads ? { threads } : undefined} />
          ) : (
            <p className="text-[13px] text-subtle">The diff isn&apos;t available.</p>
          )}
          {reviews && reviews.length > 0 && (
            <section>
              <h2 className="mb-4 text-[15px] font-semibold tracking-[-0.02em]">Reviews</h2>
              <ReviewTimeline reviews={reviews} status={status} threads={threads ?? []} />
            </section>
          )}
        </div>
        <Aside repo={repo} change={c} status={status} checks={checks} />
      </div>
    </div>
  );
}

function Aside({ repo, change: c, status, checks }: { repo: string; change: Change; status: ReviewStatus | null; checks: CheckRow[] }) {
  const p = c.preview;
  const blockers = c.state === "landed" || c.state === "closed" ? [] : (c.blockers ?? []);
  const ejected =
    c.state === "open" && c.queue?.state === "ejected" ? { reason: c.queue.error?.trim() ?? "", output: c.queue.output?.trim() ?? "" } : null;
  return (
    <aside className="min-w-0 space-y-8 text-[13px]">
      {(c.review || status) && (
        <section>
          <h2 className="text-[13px] font-semibold">Review</h2>
          <div className="mt-2">
            <ReviewStatusView summary={c.review} status={status} />
          </div>
        </section>
      )}
      {blockers.length > 0 && (
        <section>
          <h2 className="text-[13px] font-semibold">Before it can land</h2>
          <div className="mt-2">
            <BlockerList blockers={blockers} />
          </div>
        </section>
      )}
      {ejected?.reason && (
        <section>
          <h2 className="text-[13px] font-semibold">Merge queue</h2>
          <p className="mt-2 text-[12px] text-danger">Ejected: {ejected.reason}</p>
          {ejected.output && (
            <pre className="mt-2 max-h-48 overflow-auto rounded-lg border border-line bg-panel px-3 py-2 font-mono text-[11px] leading-5 whitespace-pre-wrap text-muted">
              {ejected.output}
            </pre>
          )}
        </section>
      )}
      <section>
        <h2 className="flex items-center justify-between text-[13px] font-semibold">
          Checks <ChecksSummary checks={c.checks} />
        </h2>
        {checks.length === 0 ? (
          <p className="mt-2 text-[12px] text-subtle">No checks reported.</p>
        ) : (
          <ul className="mt-2 space-y-2">
            {checks.map((ck) => (
              <li key={ck.name} className="flex items-start gap-2">
                <span className="mt-0.5">
                  <CheckIcon status={ck.status} />
                </span>
                <div className="min-w-0 flex-1">
                  <p className="truncate font-mono text-[12px]">{ck.name}</p>
                  {ck.summary && <p className={`truncate text-[11px] ${ck.errored ? "text-danger" : "text-subtle"}`}>{ck.summary}</p>}
                  <p className="truncate text-[10px] text-subtle">
                    {ck.source}
                    {ck.creator && ` · ${ck.creator}`}
                  </p>
                </div>
                {ck.url && (
                  <a href={ck.url} target="_blank" rel="noreferrer" className="text-subtle hover:text-fg" aria-label={`Open ${ck.name}`}>
                    <ExternalLink className="size-3.5" />
                  </a>
                )}
              </li>
            ))}
          </ul>
        )}
      </section>
      {c.state !== "landed" && c.state !== "closed" && (
        <section>
          <h2 className="text-[13px] font-semibold">Search this change</h2>
          <div className="mt-2">
            <SearchForm initial={{}} action={`/${repo}/search`} rev={c.head} placeholder="Search this change…" slashKey={false} />
          </div>
        </section>
      )}
      <section>
        <h2 className="text-[13px] font-semibold">Preview</h2>
        {!p ? (
          <p className="mt-2 text-[12px] text-subtle">No preview for this change.</p>
        ) : (
          <div className="mt-2 space-y-2">
            <p className="flex items-center gap-2 text-[12px] text-muted">
              <span
                className={`inline-block size-1.5 rounded-full ${
                  p.state === "running" ? "bg-ok" : p.state === "failed" ? "bg-danger" : p.state === "creating" ? "bg-accent" : "border border-line-2"
                }`}
              />
              {p.state === "sleeping" ? "scaled to zero" : p.state}
            </p>
            {p.url && (
              <a href={p.url} target="_blank" rel="noreferrer" className="flex items-center gap-1.5 truncate font-mono text-[12px] text-accent hover:underline">
                {p.url.replace(/^https?:\/\//, "")} <ExternalLink className="size-3 shrink-0" />
              </a>
            )}
            {p.postgres && (
              <p className="flex items-center gap-2 text-[12px] text-subtle">
                <Database className="size-3.5" /> Postgres branch <span className="font-mono">{p.postgres.branch_id}</span>
              </p>
            )}
            {p.redis && (
              <p className="flex items-center gap-2 text-[12px] text-subtle">
                <MemoryStick className="size-3.5" /> Redis branch <span className="font-mono">{p.redis.db_id}</span>
              </p>
            )}
            {p.error && <p className="text-[12px] text-danger">{p.error}</p>}
          </div>
        )}
      </section>
      {c.work_item && (
        <section>
          <h2 className="text-[13px] font-semibold">Work item</h2>
          <p className="mt-2 font-mono text-[12px] text-muted">{c.work_item}</p>
        </section>
      )}
    </aside>
  );
}
