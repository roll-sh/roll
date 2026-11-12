/** The overview grid's filter and the folder paths the Secrets surface walks (docs/secrets.md, "Folders"). */

import type { GridRow, RowStatus } from "./secrets-api";

export type StatusFilter = "all" | RowStatus;

/**
 * The rows that match: every word of `query` is in the key (any case; `_` and spaces both split
 * words), and the row has `status` (`all` for any).
 */
export function filterRows(rows: GridRow[], query: string, status: StatusFilter): GridRow[] {
  const words = query.toLowerCase().split(/[\s]+/).filter(Boolean);
  return rows.filter((r) => (status === "all" || r.status === status) && words.every((w) => r.key.toLowerCase().includes(w)));
}

/** How many rows have each status. */
export function countStatus(rows: GridRow[]): Record<RowStatus, number> {
  const out: Record<RowStatus, number> = { same: 0, different: 0, missing: 0 };
  for (const r of rows) out[r.status]++;
  return out;
}

/** `/api/stripe` → `[{name: "/", path: "/"}, {name: "api", path: "/api"}, {name: "stripe", path: "/api/stripe"}]`. */
export function crumbs(path: string): { name: string; path: string }[] {
  const segs = path.split("/").filter(Boolean);
  return [{ name: "/", path: "/" }, ...segs.map((name, i) => ({ name, path: `/${segs.slice(0, i + 1).join("/")}` }))];
}

/** A child folder's path. */
export const childPath = (path: string, name: string) => (path === "/" ? `/${name}` : `${path}/${name}`);

/** The last segment of a folder path (the grid's `folders` may be names or full paths). */
export const folderName = (f: string) => f.split("/").filter(Boolean).pop() ?? f;
