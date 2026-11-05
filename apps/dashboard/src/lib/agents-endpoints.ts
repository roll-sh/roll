/**
 * Endpoints in the dashboard (docs/code/agents.md, "Endpoints"): checking a spec as the daemon
 * does (agent-proto `EndpointSpec::check`), which providers can use which kind, sealing a key to a
 * machine and sending `endpoint.set`, and how a machine's info shows its endpoints (the demo lays
 * them over its providers the way the daemon does).
 */
import type { EndpointInfo, EndpointKind, EndpointModel, EndpointSpec, KeyChange, MachineInfo, ProviderId, ProviderInfo } from "./agents-api";
import { sealKey } from "./agents-seal";

export const ENDPOINT_LIMITS = { id: 64, name: 64, url: 2048, envName: 128, keyRef: 512, key: 8192, endpoints: 32 } as const;

export const KIND_LABEL: Record<EndpointKind, string> = {
  cliproxy: "CLIProxyAPI",
  anthropic: "Anthropic-compatible",
  openai: "OpenAI-compatible",
  custom: "Custom variables",
};

/** What a kind's base URL looks like, for the field's placeholder. */
export const KIND_URL: Record<EndpointKind, string> = {
  cliproxy: "http://127.0.0.1:8317",
  anthropic: "https://gateway.example.com",
  openai: "https://gateway.example.com/v1",
  custom: "https://api.example.com",
};

const ENV_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

/** The spec trimmed as the daemon keeps it, or why it can't be kept (the daemon's words). */
export function checkEndpoint(spec: EndpointSpec): { spec: EndpointSpec } | { error: string } {
  const l = ENDPOINT_LIMITS;
  if (!/^[a-z0-9_-]{1,64}$/.test(spec.id)) return { error: `an endpoint id is 1 to ${l.id} of a-z, 0-9, - and _` };
  const name = spec.name.trim();
  if (!name || new TextEncoder().encode(name).length > l.name || /[\u0000-\u001f\u007f<>]/.test(name)) {
    return { error: `an endpoint's name is one line of 1 to ${l.name} bytes, without < or >` };
  }
  const base_url = spec.base_url.trim().replace(/\/+$/, "");
  const rest = base_url.startsWith("https://") ? base_url.slice(8) : base_url.startsWith("http://") ? base_url.slice(7) : null;
  if (rest === null) return { error: "an endpoint's base URL starts with http:// or https://" };
  const authority = rest.split(/[/?]/)[0];
  if (base_url.length > l.url || !authority || /[\s\u0000-\u001f\u007f#]/.test(base_url)) {
    return { error: `an endpoint's base URL is http(s)://host[:port][/path], at most ${l.url} bytes` };
  }
  if (authority.includes("@")) return { error: "put the key in the key field, not in the URL" };
  const providers = spec.providers ?? [];
  if (new Set(providers).size !== providers.length) return { error: "list each provider once" };
  const text = (v?: string) => v?.trim() || undefined;
  const url_env = text(spec.url_env);
  const key_env = text(spec.key_env);
  const key_ref = text(spec.key_ref);
  for (const n of [url_env, key_env]) {
    if (n !== undefined && (!ENV_NAME.test(n) || n.length > l.envName)) {
      return { error: `${JSON.stringify(n)} isn't an environment variable name (letters, digits and _, not starting with a digit)` };
    }
  }
  if (spec.kind === "custom") {
    if (!url_env && !key_env) return { error: "a custom endpoint names the variable for its base URL, its key, or both" };
  } else if (url_env || key_env) {
    return { error: `only a custom endpoint names its variables (${KIND_LABEL[spec.kind]} sets its own)` };
  }
  if (key_ref !== undefined && (!key_ref.startsWith("op://") || key_ref.length > l.keyRef || /\s/.test(key_ref))) {
    return { error: `a 1Password reference looks like op://vault/item/field (at most ${l.keyRef} bytes, no spaces)` };
  }
  const out: EndpointSpec = { id: spec.id, name, kind: spec.kind, base_url };
  if (providers.length) out.providers = providers;
  if (url_env) out.url_env = url_env;
  if (key_env) out.key_env = key_env;
  if (key_ref) out.key_ref = key_ref;
  return { spec: out };
}

/** Why `provider` can't use an endpoint of `kind` (the adapters' `endpoint_use`), or null. */
export function incompatible(provider: ProviderId, kind: EndpointKind): string | null {
  if (provider === "claude" && kind === "openai") return "Claude Code speaks the Anthropic API";
  if (provider === "codex" && kind === "anthropic") return "Codex speaks the OpenAI API";
  return null;
}

/** An id for a new endpoint from its name (`cliproxy`, `team-gateway-2`), not one of `taken`. */
export function endpointId(name: string, taken: Iterable<string>): string {
  const used = new Set(taken);
  const base =
    name
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-+|-+$/g, "")
      .slice(0, 48) || "endpoint";
  if (!used.has(base)) return base;
  for (let i = 2; ; i++) if (!used.has(`${base}-${i}`)) return `${base}-${i}`;
}

/** A key typed in the browser, as `endpoint.set` takes it: sealed to the machine. */
export async function keyChange(sealKeyOf: string | undefined, endpoint: string, key: string | null | undefined): Promise<KeyChange> {
  if (key === null) return { type: "clear" };
  if (key === undefined || key === "") return { type: "keep" };
  if (key.length > ENDPOINT_LIMITS.key || /[\u0000-\u001f\u007f]/.test(key)) throw new Error(`a key is one line of 1 to ${ENDPOINT_LIMITS.key} bytes`);
  if (!sealKeyOf) throw new Error("this machine's roll agent is too old to take keys from roll: update it");
  return { type: "set", sealed: await sealKey(sealKeyOf, endpoint, key) };
}

/** The endpoint `provider` uses on a machine, if any. */
export function endpointOf(info: MachineInfo | null | undefined, provider: ProviderId): EndpointInfo | undefined {
  return info?.settings?.endpoints?.find((e) => e.providers?.includes(provider));
}

/** What `endpoint.set` does to a machine's endpoints: replaced by id, its providers taken from the others. */
export function setEndpoint(list: readonly EndpointInfo[], next: EndpointInfo): EndpointInfo[] {
  const taken = new Set(next.providers ?? []);
  const others = list
    .filter((e) => e.id !== next.id)
    .map((e) => {
      const providers = (e.providers ?? []).filter((p) => !taken.has(p));
      return providers.length ? { ...e, providers } : { ...e, providers: undefined };
    });
  return [...others, next];
}

/**
 * Providers as the daemon reports them with endpoints: each one an endpoint serves offers its
 * models after its own (`via` its name) and says "via <name>".
 */
export function withEndpoints(providers: readonly ProviderInfo[], endpoints: readonly EndpointInfo[]): ProviderInfo[] {
  return providers.map((p) => {
    const ep = endpoints.find((e) => e.providers?.includes(p.id));
    if (!ep) return p;
    const known = new Set(p.models.map((m) => m.id));
    const extra = (ep.models ?? []).filter((m) => !known.has(m.id)).map((m) => ({ id: m.id, name: m.name ?? m.id, default: false, options: [], via: ep.name }));
    return { ...p, models: [...p.models, ...extra], ...(p.installed ? { auth: "ready" as const, detail: `via ${ep.name}` } : {}) };
  });
}

/** What the demo's endpoints list (a CLIProxyAPI routes all three families). */
export function demoModels(kind: EndpointKind): EndpointModel[] {
  const claude = [{ id: "claude-opus-5-5", name: "Claude Opus 5.5" }, { id: "claude-sonnet-4-6", name: "Claude Sonnet 4.6" }];
  const openai = [{ id: "gpt-5.5" }, { id: "gpt-5.5-codex" }, { id: "o5-mini" }];
  const gemini = [{ id: "gemini-2.5-pro" }, { id: "gemini-3-flash" }];
  if (kind === "cliproxy") return [...claude, ...openai, ...gemini];
  if (kind === "anthropic") return claude;
  if (kind === "openai") return openai;
  return [];
}

/** "Key set (never shown)", "No key", "1Password: op://…". */
export function keyLabel(e: EndpointInfo): string {
  if (e.key === "reference") return `1Password: ${e.key_ref ?? ""}`;
  if (e.key === "sealed") return "Key set (never shown)";
  return "No key";
}
