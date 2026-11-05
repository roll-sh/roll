import { afterEach, describe, expect, it } from "vitest";
import type { EndpointInfo, EndpointSpec, MachineInfo, ProviderInfo } from "./agents-api";
import { AgentsClient } from "./agents-client";
import { DemoOwner } from "./agents-demo";
import { checkEndpoint, endpointId, endpointOf, incompatible, keyChange, keyLabel, setEndpoint, withEndpoints } from "./agents-endpoints";

const spec = (over: Partial<EndpointSpec> = {}): EndpointSpec => ({ id: "cliproxy", name: " CLIProxyAPI ", kind: "cliproxy", base_url: " http://100.64.0.7:8317/ ", ...over });
const error = (s: EndpointSpec) => {
  const r = checkEndpoint(s);
  return "error" in r ? r.error : null;
};

describe("endpoints", () => {
  it("checks a spec as the daemon does", () => {
    expect(checkEndpoint(spec())).toEqual({ spec: { id: "cliproxy", name: "CLIProxyAPI", kind: "cliproxy", base_url: "http://100.64.0.7:8317" } });
    expect(error(spec({ id: "Bad Id" }))).toMatch(/id/);
    expect(error(spec({ base_url: "ftp://x" }))).toMatch(/http/);
    expect(error(spec({ base_url: "https://u:sk-1@gw" }))).toMatch(/key field/);
    expect(error(spec({ base_url: "https:///v1" }))).toMatch(/host/);
    expect(error(spec({ providers: ["claude", "claude"] }))).toMatch(/once/);
    expect(error(spec({ key_env: "GEMINI_API_KEY" }))).toMatch(/only a custom/);
    expect(error(spec({ key_ref: "vault/item" }))).toMatch(/op:\/\//);
    expect(error(spec({ kind: "custom" }))).toMatch(/names the variable/);
    expect(error(spec({ kind: "custom", key_env: "1X" }))).toMatch(/variable name/);
    expect(error(spec({ kind: "custom", key_env: "GEMINI_API_KEY", key_ref: "op://Dev/Gemini/key" }))).toBeNull();
  });

  it("knows which providers can speak which kind", () => {
    expect(incompatible("claude", "openai")).toMatch(/Anthropic/);
    expect(incompatible("codex", "anthropic")).toMatch(/OpenAI/);
    expect(incompatible("claude", "cliproxy")).toBeNull();
    expect(incompatible("acp:gemini", "openai")).toBeNull();
  });

  it("names new endpoints from their names", () => {
    expect(endpointId("Team Gateway!", [])).toBe("team-gateway");
    expect(endpointId("CLIProxyAPI", ["cliproxyapi", "cliproxyapi-2"])).toBe("cliproxyapi-3");
    expect(endpointId("???", [])).toBe("endpoint");
  });

  it("seals a typed key, keeps or clears it, and never sends it in the clear", async () => {
    const pub = "KMIZzg7UJuV-Nxs3gUarEXCHzKlAkKSSSCaXrdLz4zg";
    expect(await keyChange(pub, "gw", undefined)).toEqual({ type: "keep" });
    expect(await keyChange(pub, "gw", "")).toEqual({ type: "keep" });
    expect(await keyChange(pub, "gw", null)).toEqual({ type: "clear" });
    const set = await keyChange(pub, "gw", "sk-live-1");
    expect(set.type).toBe("set");
    expect(JSON.stringify(set)).not.toContain("sk-live-1");
    await expect(keyChange(undefined, "gw", "sk-live-1")).rejects.toThrow(/update it/);
    await expect(keyChange(pub, "gw", "two\nlines")).rejects.toThrow(/one line/);
  });

  it("gives a provider one endpoint and shows its models via its name", () => {
    const a: EndpointInfo = { id: "a", name: "A", kind: "cliproxy", base_url: "http://a", providers: ["claude", "codex"], key: "sealed", models: [{ id: "gemini-2.5-pro" }, { id: "fake-1" }] };
    const b: EndpointInfo = { id: "b", name: "B", kind: "openai", base_url: "http://b/v1", providers: ["codex"], key: "none" };
    const list = setEndpoint([a], b);
    expect(list.map((e) => [e.id, e.providers])).toEqual([["a", ["claude"]], ["b", ["codex"]]]);
    const provider: ProviderInfo = {
      id: "claude", name: "Claude Code", installed: true, version: null, auth: "missing", detail: null,
      models: [{ id: "fake-1", name: "Fake 1", default: true, options: [] }], update_required: [],
      capabilities: { steer: true, fork: true, rollback: false, plan_mode: true, images: true, subagents: true, mode_switch: true },
    };
    const [p] = withEndpoints([provider], list);
    expect(p.models.map((m) => [m.id, m.via])).toEqual([["fake-1", undefined], ["gemini-2.5-pro", "A"]]);
    expect([p.auth, p.detail]).toEqual(["ready", "via A"]);
    const info = { settings: { endpoints: list } } as unknown as MachineInfo;
    expect(endpointOf(info, "codex")?.id).toBe("b");
    expect(keyLabel(a)).toBe("Key set (never shown)");
    expect(keyLabel(b)).toBe("No key");
    expect(keyLabel({ ...b, key: "reference", key_ref: "op://Dev/GW/key" })).toBe("1Password: op://Dev/GW/key");
  });
});

describe("endpoints in the demo", () => {
  let demo: DemoOwner;
  afterEach(() => demo?.dispose());

  it("sets, checks and removes endpoints like a daemon, and the picker gets their models", async () => {
    demo = new DemoOwner(0.01, false);
    const client = new AgentsClient("demo", demo, (fn) => queueMicrotask(fn));
    const release = client.hold();
    await new Promise<void>((r) => {
      const t = setInterval(() => {
        if (client.data.machines.size === 2) {
          clearInterval(t);
          r();
        }
      }, 5);
    });
    const info = () => client.data.machines.get("mch_demo_box")!.value.info!;
    const mbp = client.data.machines.get("mch_demo_ada")!.value.info!;
    expect(mbp.seal_key).toBeTruthy();
    // The seeded one: Gemini CLI through a CLIProxyAPI.
    const gemini = mbp.providers.find((p) => p.id === "acp:gemini")!;
    expect(gemini.detail).toBe("via CLIProxyAPI");
    expect(gemini.models.some((m) => m.via === "CLIProxyAPI")).toBe(true);

    const key = await keyChange(info().seal_key, "gw", "sk-demo");
    const set = { type: "endpoint.set" as const, endpoint: { id: "gw", name: "Gateway", kind: "openai" as const, base_url: "https://gw.example/v1", providers: ["codex"] }, key };
    expect(await client.send("mch_demo_box", set)).toEqual({ type: "ok" });
    await new Promise((r) => setTimeout(r, 20));
    expect(info().settings?.endpoints?.map((e) => [e.id, e.key])).toEqual([["gw", "sealed"]]);
    expect(JSON.stringify(info())).not.toContain("sk-demo");
    expect(info().providers.find((p) => p.id === "codex")?.models.some((m) => m.via === "Gateway")).toBe(true);
    expect(await client.send("mch_demo_box", { type: "endpoint.check", endpoint: "gw" })).toMatchObject({ type: "endpoint_checked", ok: true });
    const refused = await client.send("mch_demo_box", { ...set, endpoint: { ...set.endpoint, providers: ["claude"] } });
    expect(refused).toMatchObject({ type: "error", code: "invalid" });
    expect(await client.send("mch_demo_box", { type: "endpoint.remove", endpoint: "gw" })).toEqual({ type: "ok" });
    await new Promise((r) => setTimeout(r, 20));
    expect(info().settings?.endpoints).toEqual([]);
    release();
  });
});
