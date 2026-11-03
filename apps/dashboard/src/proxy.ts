/**
 * Real status codes for public roll.sh pages. Repo and owner pages stream (their data loads under
 * Suspense), and once streaming starts the status is committed as 200. So the existence check runs
 * here, before rendering: a missing owner or repo is rewritten to a path no route matches, which
 * renders `not-found.tsx` with a 404; a renamed repo gets a permanent redirect to its new name.
 * Git traffic (`.git`, `info/refs`, `git-*-pack`) and the app's own routes pass straight through.
 */

import { NextResponse, type NextRequest } from "next/server";
import { RESERVED_NAMES } from "./lib/reserved-names";

const API = process.env.ROLL_API_URL ?? process.env.NEXT_PUBLIC_ROLL_API ?? "http://127.0.0.1:7070";

/** No route matches this (three segments under `[owner]/[repo]` with no such child route). */
export const MISSING = "/_roll/missing/404";

const GIT = /^(info\/refs|git-upload-pack|git-receive-pack)$/;
const reserved = new Set<string>(RESERVED_NAMES);

export type Target = { kind: "owner"; owner: string } | { kind: "repo"; owner: string; repo: string; rest: string[] } | null;

/** Which owner or repo a path is a public page of, if any. */
export function classify(pathname: string): Target {
  let parts: string[];
  try {
    parts = pathname.split("/").filter(Boolean).map(decodeURIComponent);
  } catch {
    return null;
  }
  const [owner, repo, ...rest] = parts;
  if (!owner || reserved.has(owner.toLowerCase()) || owner.startsWith("_") || owner.includes(".")) return null;
  if (!repo) return { kind: "owner", owner };
  if (repo.toLowerCase().endsWith(".git") || GIT.test(rest.join("/"))) return null;
  return { kind: "repo", owner, repo, rest };
}

export async function proxy(request: NextRequest) {
  const t = classify(request.nextUrl.pathname);
  if (!t) return NextResponse.next();
  const enc = encodeURIComponent;
  const path = t.kind === "owner" ? `/v1/code/owners/${enc(t.owner)}` : `/v1/code/repos/${enc(t.owner)}/${enc(t.repo)}`;
  let res: Response;
  try {
    res = await fetch(`${API}${path}`, { redirect: "manual", cache: "no-store" });
  } catch {
    // The control plane is down: let the page render its offline state.
    return NextResponse.next();
  }
  if (res.status === 404) return NextResponse.rewrite(new URL(MISSING, request.url));
  if (t.kind === "repo" && (res.status === 301 || res.status === 308)) {
    const body = (await res.json().catch(() => null)) as { redirect?: string } | null;
    if (body?.redirect) {
      const url = request.nextUrl.clone();
      url.pathname = `/${body.redirect.split("/").map(enc).join("/")}${t.rest.length ? `/${t.rest.map(enc).join("/")}` : ""}`;
      return NextResponse.redirect(url, 308);
    }
  }
  return NextResponse.next();
}

export const config = {
  // Everything but build assets; `classify` skips the app's own routes without calling the API.
  matcher: ["/((?!_next/).+)"],
};
