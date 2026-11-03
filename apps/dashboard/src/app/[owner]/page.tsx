import { Link2 } from "lucide-react";
import type { Metadata } from "next";
import Link from "next/link";
import { notFound } from "next/navigation";
import { Suspense } from "react";
import { VisibilityBadge } from "@/components/code/shared";
import { Monogram } from "@/components/repo/avatar";
import { Sparkline } from "@/components/repo/meta";
import { PublicShell } from "@/components/repo/public-header";
import { languageShares, type Repo } from "@/lib/code-api";
import { getOwner, getOwnerMeta } from "@/lib/code-server";
import { requestNow } from "@/lib/now";
import { formatCount } from "@/lib/redis-api";
import { ago, formatDate } from "@/lib/time";

type Props = PageProps<"/[owner]">;

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const { owner } = await params;
  const o = await getOwnerMeta(owner);
  if (!o) return { title: owner };
  const title = o.display_name && o.display_name !== o.name ? `${o.display_name} (${o.name})` : o.name;
  const description = o.bio ?? `${o.name} on roll — ${o.repos_count} repositor${o.repos_count === 1 ? "y" : "ies"}.`;
  return { title, description, openGraph: { title, description, siteName: "roll", type: "profile" } };
}

/** A user's or organisation's page: who they are and their public repositories. */
export default function OwnerPage({ params }: Props) {
  return (
    <PublicShell>
      <main className="mx-auto w-full max-w-6xl px-4 py-12">
        <Suspense fallback={<p className="text-[13px] text-subtle"><span className="shimmer">Loading…</span></p>}>
          {params.then(({ owner }) => (
            <Profile name={owner} />
          ))}
        </Suspense>
      </main>
    </PublicShell>
  );
}

async function Profile({ name }: { name: string }) {
  const [o, now] = await Promise.all([getOwner(name), requestNow()]);
  if (!o) notFound();
  const repos = [...o.repos].sort((a, b) => b.updated_at - a.updated_at);
  const merges = repos.reduce((s, r) => s + r.stats.merges_today, 0);
  return (
    <div className="grid gap-12 md:grid-cols-[15rem_minmax(0,1fr)]">
      <aside className="min-w-0">
        <Monogram name={o.name} size={112} org={o.kind === "org"} />
        <h1 className="mt-5 text-[26px] leading-tight font-semibold tracking-[-0.05em]">{o.display_name || o.name}</h1>
        <p className="mt-0.5 font-mono text-[13px] text-subtle">
          {o.name}
          {o.kind === "org" && <span className="ml-2 rounded-full border border-line px-2 py-px font-sans text-[11px]">organisation</span>}
        </p>
        {o.bio && <p className="mt-4 text-[14px] leading-relaxed text-muted">{o.bio}</p>}
        <dl className="mt-5 space-y-1.5 text-[13px] text-muted">
          {o.url && (
            <div className="flex items-center gap-2">
              <Link2 className="size-3.5 text-subtle" />
              <a href={o.url} rel="nofollow noreferrer" target="_blank" className="truncate text-accent hover:underline">
                {o.url.replace(/^https?:\/\//, "")}
              </a>
            </div>
          )}
          <div className="text-subtle">Joined {formatDate(o.created_at)}</div>
          <div>
            <span className="font-mono text-fg tabular-nums">{formatCount(merges)}</span> {merges === 1 ? "merge" : "merges"} today
          </div>
        </dl>
      </aside>
      <section className="min-w-0">
        <h2 className="flex items-baseline gap-2 border-b border-line pb-3 text-[15px] font-semibold tracking-[-0.02em]">
          Repositories <span className="font-mono text-[12px] font-normal text-subtle">{repos.length}</span>
        </h2>
        {repos.length === 0 ? (
          <p className="py-10 text-[13px] text-subtle">No public repositories yet.</p>
        ) : (
          <ul>
            {repos.map((r) => (
              <RepoRow key={r.id} repo={r} now={now} />
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}

function RepoRow({ repo: r, now }: { repo: Repo; now: number }) {
  const lang = languageShares(r.languages)[0];
  return (
    <li className="flex items-center gap-6 border-b border-line py-5">
      <div className="min-w-0 flex-1">
        <p className="flex items-center gap-2">
          <Link href={`/${r.full_name}`} className="truncate text-[16px] font-semibold tracking-[-0.03em] hover:text-accent">
            {r.name}
          </Link>
          <VisibilityBadge visibility={r.visibility} />
        </p>
        {r.description && <p className="mt-1 line-clamp-2 text-[13px] leading-relaxed text-muted">{r.description}</p>}
        <p className="mt-2 flex flex-wrap items-center gap-x-4 gap-y-1 text-[12px] text-subtle">
          {lang && (
            <span className="flex items-center gap-1.5">
              <span className="size-2 rounded-full bg-fg/80" />
              {lang.name}
            </span>
          )}
          <span>
            <span className="font-mono tabular-nums">{formatCount(r.stats.merges_today)}</span> {r.stats.merges_today === 1 ? "merge" : "merges"} today
          </span>
          <span>updated {ago(r.updated_at, now)}</span>
        </p>
      </div>
      {r.stats.merges_daily && r.stats.merges_daily.length > 1 && (
        <div className="hidden shrink-0 text-fg sm:block">
          <Sparkline values={r.stats.merges_daily} />
        </div>
      )}
    </li>
  );
}
