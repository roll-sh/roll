/**
 * The ACP registry in a machine's settings (docs/code/agents.md, "The ACP registry"): T3 Code's
 * registry search (`AcpRegistrySearchStep.tsx`, `searchRank` in `AcpRegistrySupport.ts`,
 * `resolveOfficialAcpRegistryIconUrl` in `contracts/src/acpRegistry.ts`) over what the machine's
 * daemon answers to `registry.list`. The daemon fetches the registry and installs; this only
 * searches, labels and sends.
 */

import { commandId, type Command, type Machine, type ProviderId, type RegistryAgent, type RegistryInstalled, type Reply } from "./agents-api";

export const REGISTRY_CDN = "cdn.agentclientprotocol.com";

/** An agent's icon only from the registry's own CDN over HTTPS (T3's rule); anything else draws the fallback mark. */
export function registryIcon(icon: string | undefined | null): string | null {
  if (!icon) return null;
  try {
    const u = new URL(icon);
    return u.protocol === "https:" && u.hostname === REGISTRY_CDN && u.port === "" && u.username === "" && u.password === "" ? u.href : null;
  } catch {
    return null;
  }
}

/** Where "About" goes: the agent's site, else its repository (T3's row). */
export const aboutUrl = (a: RegistryAgent): string | null => a.website ?? a.repository ?? null;

/** T3's `searchRank`: exact, prefix, word prefixes, substrings, then the description; `undefined` doesn't match. */
export function searchRank(agent: RegistryAgent, query: string): number | undefined {
  const q = query.trim().toLowerCase();
  if (!q) return 100;
  const id = agent.id.toLowerCase();
  const name = agent.name.toLowerCase();
  const description = (agent.description ?? "").toLowerCase();
  const terms = q.split(/\s+/u);
  if (id === q || name === q) return 0;
  if (id.startsWith(q) || name.startsWith(q)) return 10;
  const words = `${id} ${name}`.split(/[^a-z0-9]+/u).filter(Boolean);
  if (terms.every((t) => words.some((w) => w.startsWith(t)))) return 20;
  if (terms.every((t) => id.includes(t) || name.includes(t))) return 30;
  if (terms.every((t) => `${id} ${name} ${description}`.includes(t))) return 50;
  return undefined;
}

/** The agents matching `query`, best first, then by name and id (T3's order). */
export function searchAgents(agents: readonly RegistryAgent[], query: string): RegistryAgent[] {
  return agents
    .flatMap((agent) => {
      const rank = searchRank(agent, query);
      return rank === undefined ? [] : [{ agent, rank }];
    })
    .sort((a, b) => a.rank - b.rank || a.agent.name.toLowerCase().localeCompare(b.agent.name.toLowerCase()) || a.agent.id.localeCompare(b.agent.id))
    .map((r) => r.agent);
}

/** What a row offers. */
export type RowState =
  | { kind: "install" }
  | { kind: "installing" }
  | { kind: "installed"; version: string }
  /** Installed at an older version; installing again updates it. */
  | { kind: "update"; from: string }
  /** The machine has it already, as another provider. */
  | { kind: "here"; provider: ProviderId }
  | { kind: "needs"; tool: "npm" | "uv" }
  | { kind: "unavailable" };

export function rowState(agent: RegistryAgent, installing: boolean): RowState {
  if (installing) return { kind: "installing" };
  if (agent.installed) return agent.installed === agent.version || !agent.installable ? { kind: "installed", version: agent.installed } : { kind: "update", from: agent.installed };
  if (agent.offered) return { kind: "here", provider: agent.offered };
  if (agent.installable) return { kind: "install" };
  const kinds = agent.kinds ?? [];
  if (kinds.includes("npx")) return { kind: "needs", tool: "npm" };
  if (kinds.includes("uvx")) return { kind: "needs", tool: "uv" };
  return { kind: "unavailable" };
}

/** The button's words for a row. */
export function rowLabel(s: RowState): string {
  switch (s.kind) {
    case "install":
      return "Install";
    case "installing":
      return "Installing…";
    case "installed":
      return "Installed";
    case "update":
      return "Update";
    case "here":
      return "On this machine";
    case "needs":
      return s.tool === "npm" ? "Needs npm" : "Needs uv";
    case "unavailable":
      return "Not for this machine";
  }
}

/** Why a machine's registry can't be used from here, or `null` when it can. */
export function registryUnavailable(machine: Machine, online: boolean): string | null {
  if (!online) return `${machine.name} isn't connected: its agents can be added once it's back.`;
  if (!machine.info?.settings?.registry) return "Update roll on this machine to add agents from the ACP registry.";
  return null;
}

/** The registry install behind a provider card, when the daemon installed it. */
export function installedFor(machine: Machine | undefined, provider: ProviderId): RegistryInstalled | undefined {
  return machine?.info?.settings?.registry?.find((i) => i.provider === provider);
}

/** How many times an install is asked again after the relay stopped waiting (30 s each): T3's 20-minute install limit. */
const INSTALL_ATTEMPTS = 40;

/**
 * Install `agent` and resolve with the daemon's reply. A download can outlast the relay's wait,
 * which answers `timeout`; asking again with the same command id waits for the same install
 * (the daemon runs a command id once and replays its reply), so nothing installs twice.
 */
export async function installAgent(send: (machine: Machine["id"], command: Command, id: string) => Promise<Reply>, machine: Machine["id"], agent: string): Promise<Reply> {
  const id = commandId();
  let reply: Reply = { type: "error", code: "timeout", message: "the machine didn't answer" };
  for (let i = 0; i < INSTALL_ATTEMPTS; i++) {
    reply = await send(machine, { type: "registry.install", agent }, id);
    if (!(reply.type === "error" && reply.code === "timeout")) return reply;
  }
  return reply;
}
