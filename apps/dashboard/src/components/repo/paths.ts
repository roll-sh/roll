/** URL builders for the public repo pages. Revisions and path segments are encoded; slashes kept. */

const seg = (path: string) => path.split("/").filter(Boolean).map(encodeURIComponent).join("/");

export const repoHref = (repo: string) => `/${repo}`;
export const treeHref = (repo: string, rev: string, path = "") =>
  `/${repo}/tree/${encodeURIComponent(rev)}${path ? `/${seg(path)}` : ""}`;
export const blobHref = (repo: string, rev: string, path: string) => `/${repo}/blob/${encodeURIComponent(rev)}/${seg(path)}`;
export const commitsHref = (repo: string, rev: string, path?: string) =>
  `/${repo}/commits/${encodeURIComponent(rev)}${path ? `?path=${encodeURIComponent(path)}` : ""}`;
export const changesetHref = (repo: string, id: string) => `/${repo}/changeset/${encodeURIComponent(id)}`;
export const changeHref = (repo: string, number: number | string) => `/${repo}/changes/${number}`;

/** A route param, decoded once (tolerates params that arrive decoded already). */
export const param = (s: string) => {
  try {
    return decodeURIComponent(s);
  } catch {
    return s;
  }
};

/** Route params (`[...path]`) back to a repo path. */
export const joinPath = (parts: string[] | undefined) => (parts ?? []).map(param).join("/");
