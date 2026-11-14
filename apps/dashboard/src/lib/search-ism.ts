/**
 * A data stream's retention on an OpenSearch-flavor cluster, which has no data stream lifecycle:
 * an Index State Management policy roll makes for the stream (`roll-retention-<stream>`). It
 * rolls the stream over, then deletes each backing index once it has been rolled over for the
 * retention (`min_rollover_age`, as Elasticsearch's lifecycle counts from a generation's rollover),
 * so the write index is never deleted; its `ism_template` puts new backing indices under it.
 */

export const RETENTION_POLICY_PREFIX = "roll-retention-";

export function retentionPolicyId(stream: string) {
  return `${RETENTION_POLICY_PREFIX}${stream}`;
}

const UNIT_MS: Record<string, number> = { s: 1_000, m: 60_000, h: 3_600_000, d: 86_400_000 };

function ms(t: string): number | null {
  const m = /^(\d+)([smhd])$/.exec(t);
  return m ? Number(m[1]) * UNIT_MS[m[2]] : null;
}

/** How old the write index gets before rolling over, from the retention as Elasticsearch's
 * lifecycle picks its `max_age=auto` (`RolloverConfiguration`): under a day 1h, under 14 days 1d,
 * under 90 days 7d, else 30d. */
export function rolloverAge(retention: string): string {
  const r = ms(retention) ?? Infinity;
  if (r < UNIT_MS.d) return "1h";
  if (r < 14 * UNIT_MS.d) return "1d";
  if (r < 90 * UNIT_MS.d) return "7d";
  return "30d";
}

/** The `PUT _plugins/_ism/policies/<id>` body of a stream's retention. */
export function retentionPolicy(stream: string, retention: string) {
  return {
    policy: {
      description: `roll: ${stream} kept ${retention}`,
      default_state: "hot",
      states: [
        {
          name: "hot",
          actions: [{ rollover: { min_index_age: rolloverAge(retention), min_primary_shard_size: "50gb" } }],
          transitions: [{ state_name: "delete", conditions: { min_rollover_age: retention } }],
        },
        { name: "delete", actions: [{ delete: {} }], transitions: [] },
      ],
      ism_template: [{ index_patterns: [`.ds-${stream}-*`], priority: 100 }],
    },
  };
}

/** The retention a roll-made policy (`GET _plugins/_ism/policies/<id>`) keeps, or `null`. */
export function retentionOf(resp: unknown): string | null {
  const states = (resp as { policy?: { states?: { name: string; transitions?: { state_name: string; conditions?: { min_rollover_age?: string } }[] }[] } } | null)?.policy?.states ?? [];
  for (const s of states) for (const t of s.transitions ?? []) if (t.state_name === "delete" && t.conditions?.min_rollover_age) return t.conditions.min_rollover_age;
  return null;
}

/** The ISM policy each backing index of a stream is under (`GET _plugins/_ism/explain/<stream>`):
 * the one they share, or `null` when none (or not all of them the same). */
export function streamPolicy(explain: unknown): string | null {
  const ids = new Set<string | null>();
  for (const [k, v] of Object.entries((explain ?? {}) as Record<string, unknown>)) {
    if (k === "total_managed_indices" || !v || typeof v !== "object") continue;
    ids.add(((v as { policy_id?: string | null }).policy_id ?? null) || null);
  }
  return ids.size === 1 ? [...ids][0] : null;
}
