import { HorizonMark } from "@roll/brand";
import { createFileRoute } from "@tanstack/react-router";
import { ShareView } from "@/components/agents/share-view";
import { loadShare, OG_IMAGE } from "@/lib/agents-server";
import { pageHead } from "@/lib/head";
import { or404 } from "@/lib/public-pages";

/** A shared thread: read-only, with a replay. An unknown or revoked id is a 404. */
export const Route = createFileRoute("/agents/s/$id/")({
  loader: async ({ params: { id } }) => or404(await loadShare(id)),
  head: ({ loaderData, params: { id } }) => {
    if (!loaderData) return {};
    const { card: c, origin } = loaderData;
    const who = [c.providers.join(" + "), c.model].filter(Boolean).join(" · ");
    const description = `${c.owner} shared a roll agents thread${who ? ` (${who})` : ""}: ${c.runs} run${c.runs === 1 ? "" : "s"}, ${c.steps} step${c.steps === 1 ? "" : "s"}. Replay it.`;
    const image = `${origin ?? ""}/agents/s/${encodeURIComponent(id)}/opengraph-image`;
    return pageHead({
      title: c.title,
      description,
      // A link someone chose to share, not a page to list.
      noindex: "nofollow",
      og: { title: c.title, description, type: "article", image: { url: image, ...OG_IMAGE } },
      twitter: { card: "summary_large_image", title: c.title, description },
    });
  },
  component: function SharePage() {
    return <ShareView share={Route.useLoaderData().share} />;
  },
  pendingComponent: () => (
    <div className="grid h-dvh place-items-center">
      <HorizonMark size={48} state="thinking" interactive={false} />
    </div>
  ),
});
