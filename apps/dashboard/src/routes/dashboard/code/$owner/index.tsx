import { createFileRoute } from "@tanstack/react-router";
import { CodeOwner } from "@/components/code/owner";
import { pageHead } from "@/lib/head";

export const Route = createFileRoute("/dashboard/code/$owner/")({
  head: ({ params }) => pageHead({ title: `${params.owner} · Code` }),
  component: function CodeOwnerPage() {
    const { owner } = Route.useParams();
    return <CodeOwner key={owner} name={owner} />;
  },
});
