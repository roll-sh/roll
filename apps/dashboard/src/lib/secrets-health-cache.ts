import { secretsApi, type Finding } from "./secrets-api";

/** How long a project's findings are reused before they're asked for again. */
const TTL_MS = 30_000;
const cache = new Map<string, { at: number; found: Promise<Finding[]> }>();

/**
 * A project's health findings, asked for at most once per {@link TTL_MS} (the sidebar's count:
 * every navigation re-renders it). A failed request isn't kept.
 */
export function cachedHealth(project: string, now = Date.now()): Promise<Finding[]> {
  const hit = cache.get(project);
  if (hit && now - hit.at < TTL_MS) return hit.found;
  const found = secretsApi(project).health();
  cache.set(project, { at: now, found });
  found.catch(() => cache.delete(project));
  return found;
}
