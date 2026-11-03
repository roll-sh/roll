import type { Metadata } from "next";
import { getRepoMeta } from "./code-server";

/**
 * Metadata for a public repo page: "<title> · owner/repo · roll" (the root layout adds "· roll"),
 * the repo description for previews, and noindex for anything that isn't public.
 */
export async function repoMetadata(owner: string, repo: string, title?: string): Promise<Metadata> {
  const r = await getRepoMeta(owner, repo);
  const full = r?.full_name ?? `${owner}/${repo}`;
  const description = r?.description ?? `${full} on roll — keep the code rolling.`;
  const fullTitle = title ? `${title} · ${full}` : full;
  return {
    title: fullTitle,
    description,
    openGraph: { title: fullTitle, description, url: r?.web_url, siteName: "roll", type: "website" },
    twitter: { card: "summary", title: fullTitle, description },
    robots: r?.visibility === "public" ? undefined : { index: false },
  };
}
