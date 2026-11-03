import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { Suspense } from "react";
import { SearchForm } from "@/components/code/search";
import { SearchResults } from "@/components/repo/search-results";
import { TableSkeleton } from "@/components/repo/sections";
import { ApiError } from "@/lib/code-api";
import { repoMetadata } from "@/lib/code-meta";
import { getRepo, serverCode } from "@/lib/code-server";

type Props = PageProps<"/[owner]/[repo]/search">;

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const { owner, repo } = await params;
  return repoMetadata(owner, repo, "Search");
}

const one = (v: string | string[] | undefined) => (Array.isArray(v) ? v[0] : v) ?? "";
const flag = (v: string | string[] | undefined) => ["1", "true", "on"].includes(one(v));

/** Code search over the default branch: literal and case-insensitive by default, regex on request. */
export default function SearchPage(props: Props) {
  return (
    <Suspense fallback={<TableSkeleton />}>
      <SearchPageContent {...props} />
    </Suspense>
  );
}

async function SearchPageContent({ params, searchParams }: Props) {
  const { owner, repo } = await params;
  const found = await getRepo(owner, repo);
  if (!found || !("repo" in found)) notFound();
  const sp = await searchParams;
  const query = { q: one(sp.q).trim(), regex: flag(sp.regex), case: flag(sp.case), path: one(sp.path).trim(), rev: one(sp.rev).trim() };
  const full = found.repo.full_name;
  const branch = found.repo.default_branch;
  return (
    <div className="space-y-4">
      <SearchForm initial={query} action={`/${full}/search`} autoFocus={!query.q} rev={query.rev || undefined} />
      {query.rev && (
        <p className="text-[12px] text-subtle">
          Searching at <span className="font-mono text-muted">{query.rev}</span> ·{" "}
          <a href={`/${full}/search?q=${encodeURIComponent(query.q)}`} className="hover:text-fg">
            search {branch} instead
          </a>
        </p>
      )}
      {query.q ? (
        <Suspense key={JSON.stringify(query)} fallback={<TableSkeleton rows={6} />}>
          <Results repo={full} query={query} branch={branch} />
        </Suspense>
      ) : (
        <p className="text-[12px] text-subtle">
          Search {query.rev ? "this revision" : "the default branch"}. Press <kbd className="rounded border border-line px-1 font-mono text-[10px]">/</kbd> anywhere in the repo to
          come back here, <kbd className="rounded border border-line px-1 font-mono text-[10px]">t</kbd> to find a file by name.
        </p>
      )}
    </div>
  );
}

async function Results({
  repo,
  query,
  branch,
}: {
  repo: string;
  query: { q: string; regex: boolean; case: boolean; path: string; rev: string };
  branch: string;
}) {
  try {
    const result = await serverCode.search(repo, {
      q: query.q,
      regex: query.regex,
      case: query.case,
      path: query.path || undefined,
      rev: query.rev || undefined,
    });
    return <SearchResults repo={repo} result={result} query={query} defaultBranch={branch} what={query.rev ? "this revision" : undefined} />;
  } catch (e) {
    if (e instanceof ApiError && (e.status === 400 || (e.status === 404 && query.rev))) {
      return <p className="rounded-xl border border-line bg-panel px-5 py-4 text-[13px] text-danger">{e.message}</p>;
    }
    throw e;
  }
}
