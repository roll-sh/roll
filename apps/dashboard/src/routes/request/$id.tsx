import { createFileRoute, notFound } from "@tanstack/react-router";
import { RequestGonePage, RequestPage } from "@/components/secrets/request-page";
import { pageHead } from "@/lib/head";
import { loadValueRequestPeek } from "@/lib/secrets-server";

/** A secret request (docs/secrets.md, "Asking for a value"): public; the values are sealed in the browser before they're sent. */
export const Route = createFileRoute("/request/$id")({
  loader: async ({ params: { id } }) => {
    const r = await loadValueRequestPeek(id);
    if (!r.peek) throw notFound();
    return { peek: r.peek, now: r.now };
  },
  // The note is the asker's; a link preview gets nothing from it.
  head: () => pageHead({ title: "A secret request", description: "Someone asked you to send a value through roll. It's encrypted in your browser before it's sent.", noindex: "nofollow" }),
  component: function Page() {
    const { peek, now } = Route.useLoaderData();
    return <RequestPage id={Route.useParams().id} peek={peek} now={now} />;
  },
  notFoundComponent: RequestGonePage,
});
