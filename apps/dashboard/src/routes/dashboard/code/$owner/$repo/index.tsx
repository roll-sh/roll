import { createFileRoute } from "@tanstack/react-router";
import { CodeRepo } from "@/components/code/repo";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/code/$owner/$repo/")({
  head: ({ params }) => pageHead({ title: `${params.owner}/${params.repo}` }),
  component: function CodeRepoPage() {
    const { owner, repo } = Route.useParams();
    return <CodeRepo key={`${owner}/${repo}`} fullName={`${owner}/${repo}`} />;
  },
});
