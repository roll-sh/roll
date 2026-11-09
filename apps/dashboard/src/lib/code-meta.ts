import type { Repo } from "./code-api";
import { pageHead } from "./head";

/**
 * The head of a public repo page: "<title> · owner/repo · roll", the repo description for
 * previews, and noindex for anything that isn't public. Nothing (the root's head) when the page is
 * a 404 and there's no repo.
 */
export function repoHead(r: Repo | undefined, title?: string) {
  if (!r) return {};
  const full = r.full_name;
  const description = r.description ?? `${full} on roll — keep the code rolling.`;
  const fullTitle = title ? `${title} · ${full}` : full;
  return pageHead({
    title: fullTitle,
    description,
    og: { title: fullTitle, description, url: r.web_url, type: "website" },
    twitter: { card: "summary", title: fullTitle, description },
    noindex: r.visibility !== "public",
  });
}
