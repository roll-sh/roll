import { createFileRoute } from "@tanstack/react-router";
import { repoTab } from "@/lib/public-pages";

/** `/dashboard/code/o/r/changes` and the other tabs as paths: the repo page on that tab. */
export const Route = createFileRoute("/dashboard/code/$owner/$repo/$tab")({
  beforeLoad: ({ params }) => repoTab(params.owner, params.repo, params.tab),
});
