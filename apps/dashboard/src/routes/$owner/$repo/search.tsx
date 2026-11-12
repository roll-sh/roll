import { createFileRoute } from "@tanstack/react-router";
import { createServerFn } from "@tanstack/react-start";
import { SearchForm } from "@/components/code/search";
import { SearchResults } from "@/components/repo/search-results";
import { TableSkeleton } from "@/components/repo/sections";
import { ApiError } from "@/lib/code-api";
import { repoHead } from "@/lib/code-meta";
import { serverCode } from "@/lib/code-server";

type Query = { q: string; regex: boolean; case: boolean; path: string; rev: string };

const flag = (v: string | undefined) => ["1", "true", "on"].includes(v ?? "");

const search = createServerFn({ method: "GET" })
  .validator((d: { repo: string; query: Query }) => d)
  .handler(async ({ data: { repo, query } }) => {
    try {
      const result = await serverCode().search(repo, {
        q: query.q,
        regex: query.regex,
        case: query.case,
        path: query.path || undefined,
        rev: query.rev || undefined,
      });
      return { result };
    } catch (e) {
      // A bad pattern, or a revision that doesn't exist: said on the page, not an error page.
      if (e instanceof ApiError && (e.status === 400 || (e.status === 404 && query.rev))) return { error: e.message };
      throw e;
    }
  });

/** Code search over the default branch: literal and case-insensitive by default, regex on request. */
export const Route = createFileRoute("/$owner/$repo/search")({
  loaderDeps: ({ search: sp }): Query => ({
    q: (sp.q ?? "").trim(),
    regex: flag(sp.regex),
    case: flag(sp.case),
    path: (sp.path ?? "").trim(),
    rev: (sp.rev ?? "").trim(),
  }),
  loader: async ({ context: { repo }, deps: query }) => (query.q ? search({ data: { repo: repo.full_name, query } }) : null),
  head: ({ match }) => repoHead(match.context.repo, "Search"),
  component: SearchPage,
  pendingComponent: () => <TableSkeleton />,
});

function SearchPage() {
  const { repo: r } = Route.useRouteContext();
  const query = Route.useLoaderDeps();
  const found = Route.useLoaderData();
  const full = r.full_name;
  const branch = r.default_branch;
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
      {!found ? (
        <p className="text-[12px] text-subtle">
          Search {query.rev ? "this revision" : "the default branch"}. Press <kbd className="rounded border border-line px-1 font-mono text-[10px]">/</kbd> anywhere in the repo to
          come back here, <kbd className="rounded border border-line px-1 font-mono text-[10px]">t</kbd> to find a file by name.
        </p>
      ) : "error" in found ? (
        <p className="rounded-xl border border-line bg-panel px-5 py-4 text-[13px] text-danger">{found.error}</p>
      ) : (
        <SearchResults repo={full} result={found.result} query={query} defaultBranch={branch} what={query.rev ? "this revision" : undefined} />
      )}
    </div>
  );
}
