/**
 * Chat ops' trigger sources and step kinds (Slack, Discord, GitHub; docs/code/chatops.md), as the
 * server's registry describes them in `GET …/automations/catalog`. `chatops-catalog.json` is
 * written from the registry itself (`ROLL_FIXTURES=write cargo test -p code-control --test
 * chatops`, which also fails while it's stale), so the demo and the template tests use the real
 * schemas. What needs setting up is per owner, so it isn't in here.
 */

import type { Catalog } from "./automations-api";
import data from "./chatops-catalog.json";

export const CHATOPS_CATALOG: Catalog = data as Catalog;

/** `c` with chat ops' entries added (each marked as needing setup when `connected` says so). */
export function withChatops(c: Catalog, connected: (integration: string) => boolean = () => true): Catalog {
  return {
    triggers: [...c.triggers, ...CHATOPS_CATALOG.triggers.map((t) => ({ ...t, needs_setup: !connected(t.source) }))],
    steps: [...c.steps, ...CHATOPS_CATALOG.steps.map((s) => ({ ...s, needs_setup: !connected(s.integration ?? "") }))],
  };
}
