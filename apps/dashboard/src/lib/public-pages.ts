/**
 * Real status codes for public roll.sh pages. A route's loader decides before anything renders,
 * and Start sends the status with the page: an unknown owner, repo or share (`/agents/s/<id>`) is a
 * 404 with the not-found page, and a renamed repo is a permanent redirect to its new name, keeping
 * the rest of the path and the query. Names the app keeps for itself never reach the API. Git
 * traffic (`.git`, `info/refs`, `git-*-pack`) is answered by server routes and never gets here.
 */

import { notFound, redirect } from "@tanstack/react-router";
import type { Repo } from "./code-api";
import type { RepoLookup } from "./code-server";
import { RESERVED_NAMES } from "./reserved-names";

const reserved = new Set<string>(RESERVED_NAMES);

/** Whether a top-level path segment can be an owner's page: not one of the app's names, `_x` or `x.y`. */
export function canBeOwner(name: string) {
  return !reserved.has(name.toLowerCase()) && !name.startsWith("_") && !name.includes(".");
}

/** A 404 for a segment that can't be an owner, before asking the API about it. */
export function ownerOr404(name: string) {
  if (!canBeOwner(name)) throw notFound();
}

const reencode = (segment: string) => {
  try {
    return encodeURIComponent(decodeURIComponent(segment));
  } catch {
    return segment;
  }
};

/**
 * The repo a lookup found, or the answer instead: a 404, or a 308 to where a renamed repo lives
 * now. `pathname` and `search` are the request's (`/owner/repo/rest…`, `?query`).
 */
export function repoOr404(found: RepoLookup, pathname: string, search: string): Repo {
  if (!found) throw notFound();
  if ("redirect" in found) {
    const rest = pathname.split("/").filter(Boolean).slice(2).map(reencode);
    const to = `/${found.redirect.split("/").map(encodeURIComponent).join("/")}${rest.length ? `/${rest.join("/")}` : ""}`;
    throw redirect({ href: `${to}${search}`, statusCode: 308 });
  }
  return found.repo;
}

/** A repo dashboard page's tabs (`#changes`). */
const REPO_TABS = new Set(["overview", "work", "changes", "queue", "code", "settings"]);

/**
 * A repo's tab typed or linked as a path (`/dashboard/code/o/r/changes`): a 307 to the repo page
 * with that tab open (`#changes`). Only the change pages under `changes/<n>` are real routes, so
 * any other segment is a 404.
 */
export function repoTab(owner: string, repo: string, tab: string): never {
  if (!REPO_TABS.has(tab)) throw notFound();
  throw redirect({ href: `/dashboard/code/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}#${tab}`, statusCode: 307 });
}

/** The value, or a 404 when there's none. */
export function or404<T>(value: T | null | undefined): T {
  if (value == null) throw notFound();
  return value;
}
