import { createFileRoute, notFound } from "@tanstack/react-router";
import { ShareGonePage, SharePage } from "@/components/secrets/share-page";
import { pageHead } from "@/lib/head";
import { loadSharePeek } from "@/lib/secrets-server";

/** A value shared once (docs/secrets.md, "Sharing a value once"): public, opened with the key in the link's fragment. */
export const Route = createFileRoute("/share/$id")({
  loader: async ({ params: { id } }) => {
    const r = await loadSharePeek(id);
    if (!r.peek) throw notFound();
    return { peek: r.peek, now: r.now };
  },
  // The note is the sharer's; a link preview gets nothing from it.
  head: () => pageHead({ title: "A shared value", description: "A value shared with you through roll. It opens a limited number of times.", noindex: "nofollow" }),
  component: function Page() {
    const { peek, now } = Route.useLoaderData();
    return <SharePage id={Route.useParams().id} peek={peek} now={now} />;
  },
  notFoundComponent: ShareGonePage,
});
