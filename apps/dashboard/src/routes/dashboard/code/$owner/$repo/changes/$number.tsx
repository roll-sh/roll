import { createFileRoute } from "@tanstack/react-router";
import { CodeChange } from "@/components/code/changes";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/code/$owner/$repo/changes/$number")({
  head: ({ params: { owner, repo, number } }) => pageHead({ title: `${number.startsWith("chg_") ? number : `#${number}`} · ${owner}/${repo}` }),
  component: function CodeChangePage() {
    const { owner, repo, number } = Route.useParams();
    return <CodeChange key={`${owner}/${repo}#${number}`} fullName={`${owner}/${repo}`} id={number} />;
  },
});
