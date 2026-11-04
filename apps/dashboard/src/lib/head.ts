/**
 * Page metadata for TanStack's `head()`: the parts of a page's head roll sets (title, description,
 * robots, OpenGraph and Twitter cards). Titles get the site suffix here (`<title> · roll`); the root
 * route's own title is plain "roll".
 */

type MetaTag = { title: string } | { name: string; content: string } | { property: string; content: string };

export interface PageMeta {
  /** Without the suffix: "Agents" becomes "Agents · roll". */
  title?: string;
  description?: string;
  /** Kept out of search results; `nofollow` also asks crawlers not to follow its links. */
  noindex?: boolean | "nofollow";
  og?: { title: string; description: string; type: "website" | "article" | "profile"; url?: string; image?: { url: string; alt: string; width: number; height: number; type: string } };
  twitter?: { card: "summary" | "summary_large_image"; title: string; description: string };
}

export const titled = (title: string) => `${title} · roll`;

export function pageHead(m: PageMeta): { meta: MetaTag[] } {
  const meta: MetaTag[] = [];
  if (m.title !== undefined) meta.push({ title: titled(m.title) });
  if (m.description !== undefined) meta.push({ name: "description", content: m.description });
  if (m.noindex) meta.push({ name: "robots", content: m.noindex === "nofollow" ? "noindex, nofollow" : "noindex" });
  if (m.og) {
    const { og } = m;
    meta.push({ property: "og:title", content: og.title }, { property: "og:description", content: og.description });
    if (og.url) meta.push({ property: "og:url", content: og.url });
    meta.push({ property: "og:site_name", content: "roll" });
    if (og.image) {
      const i = og.image;
      meta.push(
        { property: "og:image", content: i.url },
        { property: "og:image:alt", content: i.alt },
        { property: "og:image:type", content: i.type },
        { property: "og:image:width", content: String(i.width) },
        { property: "og:image:height", content: String(i.height) },
      );
    }
    meta.push({ property: "og:type", content: og.type });
  }
  if (m.twitter) {
    const t = m.twitter;
    meta.push({ name: "twitter:card", content: t.card }, { name: "twitter:title", content: t.title }, { name: "twitter:description", content: t.description });
  }
  return { meta };
}
