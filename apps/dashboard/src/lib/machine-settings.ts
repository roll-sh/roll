/**
 * Machine settings in the dashboard (docs/code/agents.md, "Machine settings"): the checks the
 * daemon makes (`MachineSettings::check` in agent-proto), so the panel says what's wrong before it
 * sends anything, T3 Code's custom-model rules (`ProviderModelsSection.tsx`), and edits that keep
 * the settings as the wire has them (unset rather than empty, providers with nothing set dropped).
 */

import { isCredentialEnv, providerSettings, type MachineSettings, type ModelInfo, type ProviderId, type ProviderInfo, type ProviderSettings } from "./agents-api";

/** agent-proto's `settings_limits`. */
export const LIMITS = { envVars: 64, envName: 128, envValue: 4096, args: 64, arg: 4096, path: 4096, models: 100, modelId: 256, name: 256, branchPrefix: 64, pullEvery: 86_400, maxDepth: 8, maxChildren: 32 } as const;

/** Why `name` can't be an environment variable set from roll, or `null`. */
export function envNameError(name: string): string | null {
  if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name) || name.length > LIMITS.envName) return `${name || "That"} isn't a variable name (letters, digits and _, not starting with a digit)`;
  if (isCredentialEnv(name)) return `${name} may hold a credential or an address, and roll never relays those: set it in the daemon's config.toml on the machine`;
  return null;
}

/** T3's custom-model checks: why `slug` can't be added to `provider`'s models, or `null`. */
export function customModelError(slug: string, provider: Pick<ProviderInfo, "models"> | undefined, custom: readonly { id: string }[]): string | null {
  const s = slug.trim();
  if (!s) return "Enter a model slug.";
  if (s.length > LIMITS.modelId) return `Model slugs must be ${LIMITS.modelId} characters or less.`;
  if (provider?.models.some((m) => m.id === s && !m.custom)) return "That model is already built in.";
  if (custom.some((m) => m.id === s)) return "That custom model is already saved.";
  return null;
}

const isPath = (p: string) => p.startsWith("/") || p === "~" || p.startsWith("~/") || /^[A-Za-z]:[\\/]/.test(p);

/** What the daemon would refuse in `set` (the first thing), or `null`. */
export function settingsError(set: MachineSettings): string | null {
  for (const [id, p] of Object.entries(set.providers ?? {})) {
    if (p.binary !== undefined && !isPath(p.binary.trim())) return `${id}'s binary must be an absolute path (or start with ~/)`;
    if ((p.args?.length ?? 0) > LIMITS.args) return `${id} takes at most ${LIMITS.args} arguments`;
    for (const name of Object.keys(p.env ?? {})) {
      const e = envNameError(name);
      if (e) return e;
    }
    if ((p.models?.length ?? 0) > LIMITS.models) return `${id} takes at most ${LIMITS.models} custom models`;
  }
  for (const [what, v] of [
    ["The worktree folder", set.worktree_dir],
    ["The clone folder", set.clone_dir],
  ] as const) {
    if (v !== undefined && !isPath(v.trim())) return `${what} must be an absolute path (or start with ~/)`;
  }
  if (set.branch_prefix !== undefined) {
    const p = set.branch_prefix.trim();
    const ok = p.length <= LIMITS.branchPrefix && !p.includes("..") && p.split("/").every((part) => part && !part.startsWith(".") && !part.endsWith(".lock") && /^[A-Za-z0-9._-]+$/.test(part));
    if (!ok) return `"${p}" can't start a branch name: use letters, digits, '-', '_', '.' and '/'`;
  }
  if ((set.git_name === undefined) !== (set.git_email === undefined)) return "Set both the git name and the git email, or neither.";
  if (set.git_email !== undefined && (!set.git_email.includes("@") || /\s/.test(set.git_email))) return "The git email must be an address.";
  return null;
}

/** `set` with `patch` laid over one provider's settings; a provider left with nothing set is dropped. */
export function withProvider(set: MachineSettings, id: ProviderId, patch: Partial<ProviderSettings>): MachineSettings {
  const next: ProviderSettings = { ...set.providers?.[id], ...patch };
  const clean = Object.fromEntries(
    Object.entries(next).filter(([k, v]) => v !== undefined && !(k === "enabled" && v === true) && !(Array.isArray(v) && v.length === 0) && !(k === "env" && Object.keys(v as object).length === 0)),
  ) as ProviderSettings;
  const providers = { ...set.providers };
  if (Object.keys(clean).length) providers[id] = clean;
  else delete providers[id];
  return without({ ...set, providers }, Object.keys(providers).length ? [] : ["providers"]);
}

/** `set` with top-level fields changed; `undefined` or an empty string unsets one. */
export function withFields(set: MachineSettings, patch: Partial<Omit<MachineSettings, "providers">>): MachineSettings {
  const next: Record<string, unknown> = { ...set, ...patch };
  for (const [k, v] of Object.entries(patch)) if (v === undefined || v === "" || (k === "defaults" && v && Object.keys(v).length === 0)) delete next[k];
  return next as MachineSettings;
}

function without(set: MachineSettings, keys: string[]): MachineSettings {
  const next: Record<string, unknown> = { ...set };
  for (const k of keys) delete next[k];
  return next as MachineSettings;
}

/**
 * What the daemon offers with these settings (as `roll agent up` reports it): the providers that
 * are on, each with its custom models after its own.
 */
export function offeredProviders(all: readonly ProviderInfo[], set: MachineSettings): ProviderInfo[] {
  return all
    .filter((p) => providerSettings(set, p.id).enabled)
    .map((p) => {
      const custom = providerSettings(set, p.id).models.filter((m) => !p.models.some((k) => k.id === m.id));
      const extra: ModelInfo[] = custom.map((m) => ({ id: m.id, name: m.name ?? m.id, default: false, options: [], custom: true }));
      return extra.length ? { ...p, models: [...p.models, ...extra] } : p;
    });
}
