/**
 * roll code for the public roll.sh pages, read on the server. Page loaders call the server
 * functions here and in their route files: in-process while a request renders, over RPC on client
 * navigations. Either way requests go to `ROLL_API_URL` anonymously, so only public repos are
 * readable. Each returns `now` too, so relative times render the same on the server and the client.
 */

import { createServerFn } from "@tanstack/react-start";
import { createCodeApi, isNotFound, type ChangesetSummary, type Owner, type Repo, type Tree } from "./code-api";
import { serverApi } from "./roll-api";

export const serverCode = () => createCodeApi(serverApi());

/** The current time in Unix seconds. */
export const nowSeconds = () => Date.now() / 1000;

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
export async function fetchRepo(owner: string, name: string): Promise<RepoLookup> {
  const res = await fetch(`${serverApi()}/v1/code/repos/${encodeURIComponent(owner)}/${encodeURIComponent(name)}`, {
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

export const lookupRepo = createServerFn({ method: "GET" })
  .validator((d: { owner: string; repo: string }) => d)
  .handler(({ data }) => fetchRepo(data.owner, data.repo));

export const loadOwner = createServerFn({ method: "GET" })
  .validator((name: string) => name)
  .handler(async ({ data: name }) => ({ owner: await orNull<Owner & { repos: Repo[] }>(serverCode().owner(name)), now: nowSeconds() }));

const README = /^readme(\.(md|markdown|mdx|txt|rst))?$/i;

export interface Doc {
  html: string;
  path: string;
}

/** A README (or any markdown file), rendered and sanitized by the API (see docs/code/http-api.md). */
export async function readDoc(repo: string, rev: string, path: string): Promise<Doc | null> {
  const doc = await orNull(serverCode().readme(repo, rev, path));
  return doc && { html: doc.html, path: doc.path };
}

export interface DirectoryData {
  tree: Tree;
  last: ChangesetSummary | null;
  readme: (Doc & { name: string }) | null;
  now: number;
}

/** A directory at a revision, its last change and its README; null when there's no such directory. */
export async function readDirectory(repo: string, rev: string, path: string): Promise<DirectoryData | null> {
  const code = serverCode();
  const [tree, log] = await Promise.all([orNull(code.tree(repo, rev, path)), orNull(code.log(repo, { rev, path: path || undefined, limit: 1 }))]);
  if (!tree) return null;
  const entry = tree.entries.find((e) => e.kind === "file" && README.test(e.name));
  const doc = entry ? await readDoc(repo, tree.changeset, entry.path) : null;
  return { tree, last: log?.items[0] ?? null, readme: doc && { ...doc, name: entry!.name }, now: nowSeconds() };
}

export const loadDirectory = createServerFn({ method: "GET" })
  .validator((d: { repo: string; rev: string; path: string }) => d)
  .handler(({ data }) => readDirectory(data.repo, data.rev, data.path));
