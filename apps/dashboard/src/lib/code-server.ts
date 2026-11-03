/**
 * roll code from server components (the public roll.sh pages). Requests go to `ROLL_API_URL`
 * anonymously, so only public repos are readable, and are deduplicated per request with React's
 * `cache`: the repo layout and its page share one fetch.
 */

import { cacheLife } from "next/cache";
import { connection } from "next/server";
import { cache } from "react";
import { createCodeApi, isNotFound, type Owner, type Repo } from "./code-api";

export const SERVER_API = process.env.ROLL_API_URL ?? process.env.NEXT_PUBLIC_ROLL_API ?? "http://127.0.0.1:7070";

/** Uncached reads must wait for a request (Cache Components); `use cache` scopes use the raw fetches below. */
export const serverCode = createCodeApi(SERVER_API, undefined, connection);

/** The value, or null on 404. */
export async function orNull<T>(p: Promise<T>): Promise<T | null> {
  try {
    return await p;
  } catch (e) {
    if (isNotFound(e)) return null;
    throw e;
  }
}

export type RepoLookup = { repo: Repo } | { redirect: string } | null;

/** A repo by owner and name; a renamed repo answers with where it lives now. */
export const getRepo = cache(async (owner: string, name: string): Promise<RepoLookup> => {
  await connection();
  return fetchRepo(owner, name);
});

async function fetchRepo(owner: string, name: string): Promise<RepoLookup> {
  const res = await fetch(`${SERVER_API}/v1/code/repos/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`, {
    cache: "no-store",
    redirect: "manual",
  });
  if (res.status === 404) return null;
  if (res.status === 301 || res.status === 308) {
    const body = (await res.json().catch(() => null)) as { redirect?: string } | null;
    const to = body?.redirect ?? res.headers.get("location")?.match(/\/v1\/code\/repos\/([^/]+\/[^/?#]+)/)?.[1];
    return to ? { redirect: decodeURIComponent(to) } : null;
  }
  if (!res.ok) throw new Error(`roll code: ${res.status} ${res.statusText}`);
  return { repo: (await res.json()) as Repo };
}

export const getOwner = cache(async (name: string) => orNull<Owner & { repos: Repo[] }>(serverCode.owner(name)));

export const getTree = cache((repo: string, rev: string, path: string) => orNull(serverCode.tree(repo, rev, path)));

export interface RepoMeta {
  full_name: string;
  description: string | null;
  web_url: string;
  visibility: Repo["visibility"];
}

/** Just what page metadata needs, cached for a minute so metadata doesn't block the shell. */
export async function getRepoMeta(owner: string, name: string): Promise<RepoMeta | null> {
  "use cache";
  cacheLife("minutes");
  const found = await fetchRepo(owner, name).catch(() => null);
  if (!found || !("repo" in found)) return null;
  const { full_name, description, web_url, visibility } = found.repo;
  return { full_name, description, web_url, visibility };
}

export interface OwnerMeta {
  name: string;
  display_name: string;
  bio: string | null;
  repos_count: number;
}

/** Just what an owner page's metadata needs, cached like `getRepoMeta`. */
export async function getOwnerMeta(name: string): Promise<OwnerMeta | null> {
  "use cache";
  cacheLife("minutes");
  const res = await fetch(`${SERVER_API}/v1/code/owners/${encodeURIComponent(name)}`).catch(() => null);
  if (!res?.ok) return null;
  const o = (await res.json()) as Owner;
  return { name: o.name, display_name: o.display_name, bio: o.bio, repos_count: o.repos_count };
}
