import type { Decoy } from "@roll/secrets/types";

/** A decoy's uses and where the last one came from (docs/secrets.md, "Decoys"), for its admins:
 * "used 3×, last from 203.0.113.7 on ep-abc123, 2m ago". `when` formats the time. */
export function decoyUse(d: Decoy, when: (ms: number) => string): string {
  const times = d.uses === 1 ? "once" : `${d.uses}×`;
  const on = d.used_endpoint ? `on ${d.used_endpoint}` : d.used_route ? `calling ${d.used_route}` : "";
  const place = [d.used_ip ? `from ${d.used_ip}` : "", on].filter(Boolean).join(" ");
  const last = [place, d.used_at === null ? "" : when(d.used_at)].filter(Boolean).join(", ");
  return last ? `used ${times}, last ${last}` : `used ${times}`;
}
