/**
 * Whether an agent can run on a machine now (docs/code/agents.md, "Providers"), the one answer
 * the machine card, a project's actions, the launcher and the folded projects all give: a
 * machine with no provider installed, or none logged in, can hold projects but can't run an
 * agent, and says what to do next instead of offering a launch that fails.
 */

import { reachable, type Machine, type ProviderInfo } from "./agents-api";

export type Readiness =
  /** Offline (and not a sandbox that wakes): nothing to set up from here. */
  | { state: "offline" }
  /** Online, not said hello yet. */
  | { state: "connecting" }
  /** Its providers are still being probed. */
  | { state: "checking" }
  /** No provider installed: install one, add one from the ACP registry, or use an endpoint. */
  | { state: "none" }
  /** Providers installed, none logged in. */
  | { state: "login"; missing: ProviderInfo[] }
  | { state: "ready" };

export function readiness(m: Machine | undefined): Readiness {
  if (!m || !reachable(m)) return { state: "offline" };
  const info = m.info;
  if (!info) return { state: "connecting" };
  const installed = info.providers.filter((p) => p.installed);
  if (installed.some((p) => p.auth !== "missing")) return { state: "ready" };
  if (info.providers.some((p) => p.checking)) return { state: "checking" };
  if (installed.length === 0) return { state: "none" };
  return { state: "login", missing: installed };
}

/** Can an agent be started there right now (or once a sleeping sandbox wakes)? */
export const canRunAgents = (m: Machine | undefined) => readiness(m).state === "ready";

/** One line for a disabled launch: why not. */
export function notReadyReason(m: Machine | undefined): string | null {
  const r = readiness(m);
  switch (r.state) {
    case "ready":
      return null;
    case "offline":
      return `${m?.name ?? "The machine"} is offline`;
    case "connecting":
      return `${m!.name} is connecting`;
    case "checking":
      return `Checking ${m!.name}'s providers`;
    case "none":
      return `No agent provider on ${m!.name} yet`;
    case "login":
      return `No provider is logged in on ${m!.name}`;
  }
}
