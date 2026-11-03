/**
 * Shares from server components (`/agents/s/<id>` and its link preview). Requests go to the API
 * anonymously: a share is public to anyone with its id. The page reads it fresh (a revoked share
 * is gone at once); metadata and the preview image use a copy cached for a minute, so they don't
 * hold up the page.
 */

import { cacheLife } from "next/cache";
import { connection } from "next/server";
import { cache } from "react";
import { shareD, type Share } from "./agents-api";
import { shareStats } from "./agents-replay";
import { SERVER_API } from "./code-server";
import { decode } from "./decode";

async function fetchShare(id: string): Promise<Share | null> {
  const res = await fetch(`${SERVER_API}/v1/code/agents/shares/${encodeURIComponent(id)}`, { cache: "no-store" });
  if (res.status === 404) return null;
  if (!res.ok) throw new Error(`roll agents: ${res.status} ${res.statusText}`);
  return decode(shareD, await res.json());
}

/** The share, or null when there's none with this id (never was, or revoked). */
export const getShare = cache(async (id: string): Promise<Share | null> => {
  await connection();
  return fetchShare(id);
});

const PROVIDERS: Record<string, string> = { claude: "Claude Code", codex: "Codex" };

/** "Claude Code", "Codex", "Gemini" (from `acp:gemini`). */
export function providerLabel(id: string): string {
  if (PROVIDERS[id]) return PROVIDERS[id];
  const name = id.startsWith("acp:") ? id.slice(4) : id;
  return name.charAt(0).toUpperCase() + name.slice(1);
}

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
export async function getShareCard(id: string): Promise<ShareCard | null> {
  "use cache";
  cacheLife("minutes");
  const s = await fetchShare(id).catch(() => null);
  if (!s) return null;
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
