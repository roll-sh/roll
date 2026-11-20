import type { SearchCluster } from "./search-api";

/**
 * Remote clusters of a search cluster (cross-cluster search, docs/search/README.md): another of
 * the person's own clusters, searched as `<alias>:<index>`. roll keeps the link's credentials
 * (`GET/PUT/DELETE /v1/search/clusters/{id}/remotes[/{alias}]`); the cluster's own
 * `_remote/info` says how each remote is set and whether it connects.
 */

/** A link roll holds credentials for (`GET …/remotes`; never the key). */
export interface RemoteLink {
  alias: string;
  /** The target's name (null: it's gone). */
  cluster: string | null;
  cluster_id: string;
  key_id: string;
  /** roll made the cross-cluster key (and revokes it on unlink). */
  minted: boolean;
  created_at: number;
  /** The target still exists and is one the cluster may link to. */
  connected: boolean;
}

/** One remote of `_remote/info`. */
export interface RemoteInfo {
  connected: boolean;
  mode: "sniff" | "proxy";
  skip_unavailable: boolean;
  proxy_address?: string;
  seeds?: string[];
  cluster_credentials?: string;
}

export type RemoteState = "connected" | "no-credentials" | "unreachable" | "unknown";

/** A row of the Remote clusters panel. */
export interface RemoteRow {
  alias: string;
  /** The cluster it reaches (a roll cluster's name, or the address it's set to). */
  target: string;
  skipUnavailable: boolean | null;
  state: RemoteState;
  /** roll holds its credentials, so it can be unlinked here. */
  linked: boolean;
  minted: boolean;
}

/** An alias as `cluster.remote.<alias>.*` takes it (search-control `valid_alias`). */
export const ALIAS_RE = /^[A-Za-z0-9_-]{1,64}$/;

export function validAlias(alias: string): boolean {
  return ALIAS_RE.test(alias);
}

/**
 * The rows: every link roll holds, with what `_remote/info` says of it when that was read
 * (`info` null: not read, the cluster may be asleep), plus remotes set in the cluster's settings
 * that roll holds no credentials for.
 */
export function remoteRows(links: RemoteLink[], info: Record<string, RemoteInfo> | null): RemoteRow[] {
  const rows: RemoteRow[] = links.map((l) => {
    const i = info?.[l.alias];
    const state: RemoteState = !l.connected ? "unreachable" : !info ? "unknown" : i?.connected ? "connected" : "unreachable";
    return { alias: l.alias, target: l.cluster ?? "deleted cluster", skipUnavailable: i ? i.skip_unavailable : null, state, linked: true, minted: l.minted };
  });
  for (const [alias, i] of Object.entries(info ?? {})) {
    if (links.some((l) => l.alias === alias)) continue;
    const target = i.mode === "proxy" ? (i.proxy_address ?? "") : (i.seeds ?? []).join(", ");
    rows.push({ alias, target, skipUnavailable: i.skip_unavailable, state: i.connected ? "connected" : "no-credentials", linked: false, minted: false });
  }
  return rows.sort((a, b) => a.alias.localeCompare(b.alias));
}

export function stateLabel(s: RemoteState): string {
  switch (s) {
    case "connected":
      return "Connected";
    case "no-credentials":
      return "No credentials";
    case "unreachable":
      return "Unreachable";
    case "unknown":
      return "Linked";
  }
}

/** The clusters `self` may link to: the person's other clusters of its flavor. */
export function linkTargets(clusters: SearchCluster[], self: SearchCluster): SearchCluster[] {
  return clusters.filter((c) => c.id !== self.id && c.flavor === self.flavor && (c.owner ?? null) === (self.owner ?? null)).sort((a, b) => a.name.localeCompare(b.name));
}

/** An alias for a new link to `target`: its name, unless the cluster already has that alias. */
export function suggestAlias(target: string, taken: string[]): string {
  let alias = target.replace(/[^A-Za-z0-9_-]/g, "-").slice(0, 64);
  for (let n = 2; taken.includes(alias); n++) alias = `${target.slice(0, 60)}-${n}`;
  return alias;
}

/** Index patterns, comma- or space-separated (empty: every index). */
export function parsePatterns(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((p) => p.trim())
    .filter(Boolean);
}

/** The body of `PUT …/remotes/{alias}` (roll makes the target's key with `access`). */
export interface LinkBody {
  cluster: string;
  skip_unavailable: boolean;
  access: { search: { names: string[] }[] };
}

/** A link to `target` whose key searches `patterns`. */
export function linkBody(target: string, skipUnavailable: boolean, patterns: string[]): LinkBody {
  return { cluster: target, skip_unavailable: skipUnavailable, access: { search: [{ names: patterns.length ? patterns : ["*"] }] } };
}
