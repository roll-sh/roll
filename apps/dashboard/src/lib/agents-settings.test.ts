import { describe, expect, it } from "vitest";
import { isCredentialEnv } from "./agents-api";
import { customModelError, envNameError, parseAutoCompact, settingsError, withFields, withProvider } from "./machine-settings";
import { memorySettings } from "./agents-services";
import { DEFAULT_OWNER_SETTINGS, newThreadDefaults, parseOwnerSettings } from "./owner-settings";
import { notices, seen } from "./agents-notify";
import type { StreamEvent } from "./agents-api";
import { applyEvents, emptyData } from "./agents-store";
import { approval, mirrored, provider, thread } from "@/test/agents";

describe("machine settings in the dashboard", () => {
  it("refuses what the daemon refuses, before sending it", () => {
    expect(isCredentialEnv("OPENAI_API_KEY") && isCredentialEnv("ANTHROPIC_BASE_URL") && !isCredentialEnv("CODEX_HOME")).toBe(true);
    expect(envNameError("CODEX_HOME")).toBeNull();
    expect(envNameError("1X")).toMatch(/isn't a variable name/);
    expect(envNameError("GH_TOKEN")).toMatch(/config.toml/);
    expect(settingsError({ providers: { claude: { env: { ANTHROPIC_API_KEY: "x" } } } })).toMatch(/never relays/);
    expect(settingsError({ worktree_dir: "relative" })).toMatch(/absolute/);
    expect(settingsError({ branch_prefix: "a..b" })).toMatch(/branch name/);
    expect(settingsError({ git_name: "Ada" })).toMatch(/both/);
    expect(settingsError({ branch_prefix: "ada/wip", worktree_dir: "~/wt", providers: { codex: { binary: "/opt/codex" } } })).toBeNull();
  });

  it("T3's auto-compact window: empty is the default, 100,000 to 1,000,000 otherwise", () => {
    expect(parseAutoCompact("")).toEqual({ ok: true, value: undefined });
    expect(parseAutoCompact(" 300,000 ")).toEqual({ ok: true, value: 300_000 });
    expect(parseAutoCompact("1000000")).toEqual({ ok: true, value: 1_000_000 });
    for (const bad of ["99999", "1000001", "3e5", "abc", "-200000", "250000.5"]) expect(parseAutoCompact(bad).ok, bad).toBe(false);
    expect(settingsError({ providers: { claude: { auto_compact_window: 5 } } })).toBe("claude compacts after 100,000 to 1,000,000 tokens");
  });

  it("T3's custom-model rules", () => {
    const p = provider();
    expect(customModelError(" ", p, [])).toBe("Enter a model slug.");
    expect(customModelError("claude-opus-5-5", p, [])).toBe("That model is already built in.");
    expect(customModelError("x", p, [{ id: "x" }])).toBe("That custom model is already saved.");
    expect(customModelError("m".repeat(257), p, [])).toMatch(/256 characters/);
    // A custom model the daemon already lists (custom: true) isn't "built in".
    expect(customModelError("big", { models: [{ id: "big", name: "Big", default: false, options: [], custom: true }] }, [])).toBeNull();
  });

  it("edits keep the wire's shape: unset, not empty; a provider with nothing set goes", () => {
    const off = withProvider({}, "codex", { enabled: false });
    expect(off).toEqual({ providers: { codex: { enabled: false } } });
    expect(withProvider(off, "codex", { enabled: true })).toEqual({});
    expect(withProvider({}, "claude", { args: [], env: {}, binary: undefined })).toEqual({});
    expect(withFields({ branch_prefix: "ada", pull_every: 300 }, { branch_prefix: "" })).toEqual({ pull_every: 300 });
    expect(withFields({ defaults: { worktree: true } }, { defaults: {} })).toEqual({});
  });

});

describe("the owner's settings", () => {
  it("parse leniently: bad or missing values are their defaults", () => {
    expect(parseOwnerSettings(null)).toEqual(DEFAULT_OWNER_SETTINGS);
    expect(parseOwnerSettings({ notifications: "loud", send: "mod_enter", confirm_delete: "yes", defaults: { model: "x", runtime_mode: "ask", worktree: 1 } })).toEqual({
      ...DEFAULT_OWNER_SETTINGS,
      send: "mod_enter",
      // A model without its provider means nothing.
      defaults: { runtime_mode: "ask" },
    });
  });

  it("new threads: each field from the first layer that sets it; provider, model and effort together, from one on this machine", () => {
    const project = { worktree: false };
    const machine = { provider: "gemini", model: "g-3", runtime_mode: "ask" as const };
    const owner = { provider: "claude", model: "claude-sonnet-5", effort: "high", runtime_mode: "full_access" as const, interaction_mode: "plan" as const };
    expect(newThreadDefaults([project, machine, owner])).toEqual({ provider: "gemini", model: "g-3", runtime_mode: "ask", interaction_mode: "plan", worktree: false });
    expect(newThreadDefaults([project, machine, owner], ["claude", "codex"])).toEqual({
      provider: "claude",
      model: "claude-sonnet-5",
      effort: "high",
      runtime_mode: "ask",
      interaction_mode: "plan",
      worktree: false,
    });
    expect(newThreadDefaults([undefined, undefined, {}])).toEqual({});
  });

  it("in memory as the relay keeps them: revisions, conflicts and the 32 KiB limit", async () => {
    const docs = memorySettings();
    expect(await docs.load("acme")).toBeNull();
    expect(await docs.save("acme", { send: "enter" }, 0)).toEqual({ revision: 1 });
    await expect(docs.save("acme", { send: "x" }, 0)).rejects.toThrow(/revision 1/);
    await expect(docs.save("acme", [] as unknown, 1)).rejects.toThrow(/JSON object/);
    await expect(docs.save("acme", { f: "x".repeat(33 * 1024) })).rejects.toThrow(/32768/);
    expect(await docs.load("acme")).toEqual({ doc: { send: "enter" }, revision: 1 });
  });
});

describe("notifications", () => {
  it("tell about new requests and finished threads, not what was already there, nor subagents", () => {
    const busy = thread({ status: "running", active_run: "run_1" });
    const sub = thread({ id: "thr_s", status: "running", lineage: { parent: "thr_1", relation: "subagent", root: "thr_1", from_run: null } });
    const ev = (e: Parameters<typeof mirrored>) => ({ type: "entity", ...mirrored(...e) }) as StreamEvent;
    let data = applyEvents(emptyData(), [ev(["thread", busy, 1]), ev(["thread", sub, 2])]);
    const before = seen(data);
    expect(notices(before, data)).toEqual([]);
    data = applyEvents(data, [
      ev(["thread", { ...busy, status: "idle", active_run: null }, 3]),
      ev(["thread", { ...sub, status: "idle" }, 4]),
      ev(["thread", thread({ id: "thr_2", title: "Upgrade", status: "waiting", pending_requests: 1 }), 5]),
      ev(["request", approval({ thread: "thr_2" }), 6]),
    ]);
    const found = notices(before, data);
    expect(found.map((n) => [n.title, n.thread])).toEqual([
      ["An agent needs your approval", "thr_2"],
      ["An agent finished", "thr_1"],
    ]);
    expect(notices(seen(data), data)).toEqual([]);
  });
});
