import { createFileRoute } from "@tanstack/react-router";
import { Breadcrumbs } from "@/components/repo/breadcrumbs";
import { RefPicker } from "@/components/repo/ref-picker";
import { Directory, TableSkeleton } from "@/components/repo/sections";
import { repoHead } from "@/lib/code-meta";
import { loadDirectory } from "@/lib/code-server";
import { or404 } from "@/lib/public-pages";

/** A directory at any ref or changeset (`/tree/<rev>` is its root). */
export const Route = createFileRoute("/$owner/$repo/tree/$rev/$")({
  loader: async ({ context: { repo }, params: { rev, _splat: path = "" } }) =>
    or404(await loadDirectory({ data: { repo: repo.full_name, rev, path } })),
  head: ({ match, params: { repo, rev, _splat: path } }) => repoHead(match.context.repo, `${path || repo} at ${rev}`),
  component: TreePage,
  pendingComponent: () => <TableSkeleton />,
});

function TreePage() {
  const { repo: r } = Route.useRouteContext();
  const { rev, _splat: path = "" } = Route.useParams();
  const data = Route.useLoaderData();
  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-3">
        <RefPicker repo={r.full_name} rev={rev} kind="tree" path={path} />
        <Breadcrumbs repo={r.full_name} rev={rev} path={path} />
      </div>
      <Directory repo={r} rev={rev} path={path} data={data} />
    </div>
  );
}
