import { afterEach, describe, expect, it, vi } from "vitest";
import type { Machine, RegistryAgent, Reply } from "./agents-api";
import { AgentsClient } from "./agents-client";
import { DemoOwner } from "./agents-demo";
import { installAgent, installedFor, registryIcon, registryUnavailable, rowLabel, rowState, searchAgents } from "./agents-registry";

const agent = (over: Partial<RegistryAgent> = {}): RegistryAgent => ({ id: "goose", name: "goose", version: "1.57.0", kinds: ["binary"], installable: true, integrity: "sha256", ...over });

describe("the ACP registry", () => {
  it("draws icons only from the registry's CDN over HTTPS", () => {
    expect(registryIcon("https://cdn.agentclientprotocol.com/registry/v1/latest/goose.svg")).toBe("https://cdn.agentclientprotocol.com/registry/v1/latest/goose.svg");
    for (const bad of ["http://cdn.agentclientprotocol.com/x.svg", "https://evil.example/x.svg", "https://cdn.agentclientprotocol.com:444/x.svg", "https://u:p@cdn.agentclientprotocol.com/x.svg", "javascript:alert(1)", "not a url", undefined]) {
      expect(registryIcon(bad)).toBeNull();
    }
  });

  it("ranks a search like T3: exact, prefix, word prefix, substring, description", () => {
    const agents = [
      agent({ id: "kimi", name: "Kimi CLI", description: "Moonshot's agent" }),
      agent({ id: "github-copilot-cli", name: "GitHub Copilot" }),
      agent({ id: "goose", name: "goose", description: "An open source agent from Block" }),
      agent({ id: "mistral-vibe", name: "Mistral Vibe" }),
      agent({ id: "cline", name: "Cline" }),
    ];
    const ids = (q: string) => searchAgents(agents, q).map((a) => a.id);
    expect(ids("")).toEqual(["cline", "github-copilot-cli", "goose", "kimi", "mistral-vibe"]);
    expect(ids("goose")).toEqual(["goose"]);
    expect(ids("vibe")).toEqual(["mistral-vibe"]);
    expect(ids("cli")).toEqual(["cline", "github-copilot-cli", "kimi"]);
    expect(ids("block")).toEqual(["goose"]);
    expect(ids("nothing like it")).toEqual([]);
  });

  it("says what each row offers", () => {
    const label = (a: RegistryAgent, installing = false) => rowLabel(rowState(a, installing));
    expect(label(agent())).toBe("Install");
    expect(label(agent(), true)).toBe("Installing…");
    expect(label(agent({ installed: "1.57.0", offered: "acp:goose" }))).toBe("Installed");
    expect(rowState(agent({ installed: "1.50.0" }), false)).toEqual({ kind: "update", from: "1.50.0" });
    expect(label(agent({ offered: "acp:copilot" }))).toBe("On this machine");
    expect(label(agent({ kinds: ["npx"], installable: false }))).toBe("Needs npm");
    expect(label(agent({ kinds: ["uvx"], installable: false }))).toBe("Needs uv");
    expect(label(agent({ kinds: [], installable: false }))).toBe("Not for this machine");
  });

  it("is read-only for a machine that's offline or too old", () => {
    const m = (registry: boolean) => ({ id: "mch_1", name: "box", info: { settings: registry ? { registry: [] } : {} } }) as unknown as Machine;
    expect(registryUnavailable(m(true), true)).toBeNull();
    expect(registryUnavailable(m(true), false)).toMatch(/isn't connected/);
    expect(registryUnavailable(m(false), true)).toMatch(/Update roll/);
    const installed = { id: "goose", version: "1", provider: "acp:goose", kind: "binary" as const, installed_at: 1 };
    const withOne = { id: "mch_1", name: "box", info: { settings: { registry: [installed] } } } as unknown as Machine;
    expect(installedFor(withOne, "acp:goose")).toEqual(installed);
    expect(installedFor(withOne, "acp:kimi")).toBeUndefined();
  });

  it("asks again with the same command id while the relay times out", async () => {
    const replies: Reply[] = [{ type: "error", code: "timeout", message: "30 s" }, { type: "error", code: "timeout", message: "30 s" }, { type: "ok" }];
    const send = vi.fn(async () => replies.shift()!);
    expect(await installAgent(send, "mch_1", "goose")).toEqual({ type: "ok" });
    expect(send).toHaveBeenCalledTimes(3);
    const ids = new Set(send.mock.calls.map((c) => (c as unknown as [string, unknown, string])[2]));
    expect(ids.size).toBe(1);
    expect(send.mock.calls[0]).toEqual(["mch_1", { type: "registry.install", agent: "goose" }, expect.any(String)]);
    const failing = vi.fn(async (): Promise<Reply> => ({ type: "error", code: "provider", message: "SHA-256 mismatch" }));
    expect(await installAgent(failing, "mch_1", "bad")).toMatchObject({ code: "provider" });
    expect(failing).toHaveBeenCalledTimes(1);
  });
});

describe("the ACP registry in the demo", () => {
  let demo: DemoOwner;
  afterEach(() => demo?.dispose());

  it("lists, installs and removes agents like a daemon", async () => {
    demo = new DemoOwner(0.01, false);
    const client = new AgentsClient("demo", demo, (fn) => queueMicrotask(fn));
    const release = client.hold();
    await vi.waitFor(() => expect(client.data.machines.has("mch_demo_box")).toBe(true));
    const box = () => client.data.machines.get("mch_demo_box")!.value.info!;
    expect(box().settings?.registry).toEqual([]);

    const listed = await client.send("mch_demo_box", { type: "registry.list" });
    if (listed.type !== "registry") throw new Error(listed.type);
    expect(listed.agents.find((a) => a.id === "goose")).toMatchObject({ installable: true, integrity: "sha256" });
    expect(rowLabel(rowState(listed.agents.find((a) => a.id === "fast-agent")!, false))).toBe("Needs uv");
    // ada-mbp has OpenCode natively: the registry's is "On this machine".
    const mbp = await client.send("mch_demo_ada", { type: "registry.list" });
    if (mbp.type !== "registry") throw new Error(mbp.type);
    expect(mbp.agents.find((a) => a.id === "opencode")?.offered).toBe("opencode");
    expect(await client.send("mch_demo_ada", { type: "registry.install", agent: "opencode" })).toMatchObject({ type: "error", code: "conflict" });

    expect(await installAgent((m, c, id) => client.send(m, c, id), "mch_demo_box", "goose")).toEqual({ type: "ok" });
    await vi.waitFor(() => expect(box().providers.some((p) => p.id === "acp:goose" && p.installed)).toBe(true));
    expect(box().settings?.registry?.map((i) => i.id)).toEqual(["goose"]);
    expect(box().settings?.providers.some((p) => p.id === "acp:goose")).toBe(true);
    const again = await client.send("mch_demo_box", { type: "registry.list" });
    if (again.type !== "registry") throw new Error(again.type);
    expect(rowLabel(rowState(again.agents.find((a) => a.id === "goose")!, false))).toBe("Installed");

    expect(await client.send("mch_demo_box", { type: "registry.remove", agent: "goose" })).toEqual({ type: "ok" });
    await vi.waitFor(() => expect(box().providers.some((p) => p.id === "acp:goose")).toBe(false));
    expect(box().settings?.registry).toEqual([]);
    release();
  });
});
