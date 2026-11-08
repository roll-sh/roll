/**
 * Shares on the server (`/agents/s/<id>` and its link preview). Requests go to the API
 * anonymously: a share is public to anyone with its id. Both read it fresh, so a revoked share is
 * gone at once.
 */

import { createServerFn } from "@tanstack/react-start";
import { getRequestHeader } from "@tanstack/react-start/server";
import { shareD, type Share } from "./agents-api";
import { providerLabel, shareStats } from "./agents-replay";
import { serverApi } from "./roll-api";
import { decode } from "./decode";

/** JSON as the API sends it: what a server function hands the page before it's decoded. */
export type Json = string | number | boolean | null | Json[] | { [key: string]: Json };

async function fetchShareJson(id: string): Promise<Json | null> {
  const res = await fetch(`${serverApi()}/v1/code/agents/shares/${encodeURIComponent(id)}`, { cache: "no-store" });
  if (res.status === 404) return null;
  if (!res.ok) throw new Error(`roll agents: ${res.status} ${res.statusText}`);
  return (await res.json()) as Json;
}

/** The share, or null when there's none with this id (never was, or revoked). */
export async function fetchShare(id: string): Promise<Share | null> {
  const json = await fetchShareJson(id);
  return json === null ? null : decode(shareD, json);
}

/** The link preview (`/agents/s/<id>/opengraph-image`). */
export const OG_IMAGE = { width: 1200, height: 630, type: "image/png", alt: "A shared roll agents thread" } as const;

export interface ShareCard {
  title: string;
  owner: string;
  machine: string;
  os: string | null;
  runs: number;
  steps: number;
  threads: number;
  providers: string[];
  model: string | null;
  /** The kinds of the first nodes, in order (the preview draws a dot for each). */
  marks: ("text" | "tool" | "ask" | "agent")[];
  created_at: number;
}

/** What metadata and the preview image show. */
export function shareCard(s: Share): ShareCard {
  const st = shareStats(s);
  const root = s.entities.find((e) => e.kind === "thread" && e.id === s.thread);
  const marks: ShareCard["marks"] = [];
  for (const e of s.entities) {
    if (e.kind !== "node" || marks.length >= 40) continue;
    const t = e.body.type;
    marks.push(t === "tool" ? "tool" : t === "request" ? "ask" : t === "subagent" ? "agent" : "text");
  }
  return {
    title: s.title,
    owner: s.owner,
    machine: s.machine.name,
    os: s.machine.os,
    runs: st.runs,
    steps: st.steps,
    threads: st.threads,
    providers: st.providers.map(providerLabel),
    model: root?.kind === "thread" ? root.model : null,
    marks,
    created_at: s.created_at,
  };
}

/** Where this request came in, so the preview image's URL is absolute on the right host. */
function requestOrigin(): string | null {
  const host = getRequestHeader("x-forwarded-host") ?? getRequestHeader("host");
  if (!host) return null;
  const proto = getRequestHeader("x-forwarded-proto")?.split(",")[0].trim() || (/^(localhost|127\.|\[::1\])/.test(host) ? "http" : "https");
  try {
    return new URL(`${proto}://${host}`).origin;
  } catch {
    return null;
  }
}

/** A share as the API sent it, and the origin the preview image is on. */
const loadShareJson = createServerFn({ method: "GET" })
  .validator((id: string) => id)
  .handler(async ({ data: id }) => ({ json: await fetchShareJson(id), origin: requestOrigin() }));

/** The share page's data: the share, its card for metadata, and the preview's origin; null for none. */
export async function loadShare(id: string) {
  const { json, origin } = await loadShareJson({ data: id });
  if (json === null) return null;
  const share = decode(shareD, json);
  return { share, card: shareCard(share), origin };
}
