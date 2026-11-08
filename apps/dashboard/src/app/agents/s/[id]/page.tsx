import type { Metadata } from "next";
import { headers } from "next/headers";
import { notFound } from "next/navigation";
import { Suspense } from "react";
import { HorizonMark } from "@roll/brand";
import { ShareView } from "@/components/agents/share-view";
import { getShare, getShareCard } from "@/lib/agents-server";

type Props = PageProps<"/agents/s/[id]">;

/** Where this request came in, so the preview image's URL is absolute on the right host. */
async function origin(): Promise<URL | undefined> {
  const h = await headers();
  const host = h.get("x-forwarded-host") ?? h.get("host");
  if (!host) return undefined;
  const proto = h.get("x-forwarded-proto")?.split(",")[0].trim() || (/^(localhost|127\.|\[::1\])/.test(host) ? "http" : "https");
  try {
    return new URL(`${proto}://${host}`);
  } catch {
    return undefined;
  }
}

export async function generateMetadata({ params }: Props): Promise<Metadata> {
  const { id } = await params;
  const [c, metadataBase] = await Promise.all([getShareCard(id), origin()]);
  if (!c) return { title: "Shared thread", robots: { index: false } };
  const who = [c.providers.join(" + "), c.model].filter(Boolean).join(" · ");
  const description = `${c.owner} shared a roll agents thread${who ? ` (${who})` : ""}: ${c.runs} run${c.runs === 1 ? "" : "s"}, ${c.steps} step${c.steps === 1 ? "" : "s"}. Replay it.`;
  return {
    metadataBase,
    title: c.title,
    description,
    // A link someone chose to share, not a page to list.
    robots: { index: false, follow: false },
    openGraph: { title: c.title, description, siteName: "roll", type: "article" },
    twitter: { card: "summary_large_image", title: c.title, description },
  };
}

/**
 * A shared thread: read-only, with a replay. `src/proxy.ts` has already answered 404 for an
 * unknown id; this only renders what exists.
 */
export default function SharePage({ params }: Props) {
  return (
    <Suspense
      fallback={
        <div className="grid h-dvh place-items-center">
          <HorizonMark size={48} state="thinking" interactive={false} />
        </div>
      }
    >
      {params.then(({ id }) => (
        <Loaded id={id} />
      ))}
    </Suspense>
  );
}

async function Loaded({ id }: { id: string }) {
  const share = await getShare(id);
  if (!share) notFound();
  return <ShareView share={share} />;
}
