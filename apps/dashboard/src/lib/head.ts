/**
 * Page metadata for TanStack's `head()`: the parts of a page's head roll sets (title, description,
 * robots, OpenGraph and Twitter cards), in the tags and order Next.js wrote them. Titles get the
 * site suffix here (`<title> · roll`); the root route's own title is plain "roll". Twitter's card
 * takes whatever it doesn't set from OpenGraph.
 */

type MetaTag = { title: string } | { name: string; content: string } | { property: string; content: string };

interface Image {
  url: string;
  alt: string;
  width: number;
  height: number;
  type: string;
}

export interface PageMeta {
  /** Without the suffix: "Agents" becomes "Agents · roll". */
  title?: string;
  description?: string;
  /** Kept out of search results; `nofollow` also asks crawlers not to follow its links. */
  noindex?: boolean | "nofollow";
  og?: { title: string; description: string; type: "website" | "article" | "profile"; url?: string; image?: Image };
  twitter?: { card: "summary" | "summary_large_image"; title?: string; description?: string };
}

export const titled = (title: string) => `${title} · roll`;

export function pageHead(m: PageMeta): { meta: MetaTag[] } {
  const meta: MetaTag[] = [];
  if (m.title !== undefined) meta.push({ title: titled(m.title) });
  if (m.description !== undefined) meta.push({ name: "description", content: m.description });
  if (m.noindex) meta.push({ name: "robots", content: m.noindex === "nofollow" ? "noindex, nofollow" : "noindex" });
  const { og } = m;
  if (og) {
    meta.push({ property: "og:title", content: og.title }, { property: "og:description", content: og.description });
    if (og.url) meta.push({ property: "og:url", content: og.url });
    meta.push({ property: "og:site_name", content: "roll" });
    if (og.image) meta.push(...imageTags("property", "og:image", og.image));
    meta.push({ property: "og:type", content: og.type });
  }
  if (m.twitter || og) {
    const card = m.twitter?.card ?? (og?.image ? "summary_large_image" : "summary");
    const title = m.twitter?.title ?? og?.title;
    const description = m.twitter?.description ?? og?.description;
    meta.push({ name: "twitter:card", content: card });
    if (title !== undefined) meta.push({ name: "twitter:title", content: title });
    if (description !== undefined) meta.push({ name: "twitter:description", content: description });
    if (og?.image) meta.push(...imageTags("name", "twitter:image", og.image));
  }
  return { meta };
}

function imageTags(key: "property" | "name", prefix: string, i: Image): MetaTag[] {
  const tag = (suffix: string, content: string) => ({ [key]: `${prefix}${suffix}`, content }) as MetaTag;
  return [tag("", i.url), tag(":type", i.type), tag(":width", String(i.width)), tag(":height", String(i.height)), tag(":alt", i.alt)];
}
