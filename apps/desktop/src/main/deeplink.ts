/**
 * `roll://` deep links: `roll://agents`, `roll://agents/thread/<id>`, `roll://agents/connect?code=…`.
 * Links come from other programs, so anything that doesn't match exactly is dropped.
 */
import type { DeepLink } from "../shared/bridge.ts";

export const SCHEME = "roll";

const THREAD_ID = /^[A-Za-z0-9_-]{1,128}$/;
const USER_CODE = /^[A-Za-z0-9-]{1,32}$/;

export function parseDeepLink(raw: string): DeepLink | null {
  if (raw.length > 2048) return null;
  let url: URL;
  try {
    url = new URL(raw);
  } catch {
    return null;
  }
  if (url.protocol !== `${SCHEME}:` || url.host !== "agents" || url.username || url.password) return null;
  const parts = url.pathname.split("/").filter(Boolean);
  if (parts.length === 0) return { kind: "agents" };
  if (parts.length === 2 && parts[0] === "thread") {
    const thread = decodeSegment(parts[1]);
    return thread !== null && THREAD_ID.test(thread) ? { kind: "thread", thread } : null;
  }
  if (parts.length === 1 && parts[0] === "connect") {
    const code = url.searchParams.get("code");
    return code !== null && USER_CODE.test(code) ? { kind: "connect", code } : null;
  }
  return null;
}

function decodeSegment(s: string): string | null {
  try {
    return decodeURIComponent(s);
  } catch {
    return null;
  }
}

/** The dashboard path a link opens when the page doesn't handle it itself. */
export function deepLinkPath(link: DeepLink): string {
  switch (link.kind) {
    case "agents":
      return "/dashboard/agents";
    case "thread":
      // The canvas focuses (and opens) a node from `?focus=t:<thread>`.
      return `/dashboard/agents?focus=${encodeURIComponent(`t:${link.thread}`)}`;
    case "connect":
      return `/dashboard/agents/connect?code=${encodeURIComponent(link.code)}`;
  }
}

/** The deep link in a process's arguments (Windows and Linux pass it on the command line). */
export function deepLinkFromArgv(argv: readonly string[]): DeepLink | null {
  for (const arg of argv) {
    if (arg.startsWith(`${SCHEME}://`)) {
      const link = parseDeepLink(arg);
      if (link) return link;
    }
  }
  return null;
}
