import { describe, expect, it, vi } from "vitest";
import { createIntegrationsApi, NO_INTEGRATIONS, parseIntegrations, parseIntegrationsLink, safeUrl } from "./integrations-api";
import { demoIntegrations } from "./agents-demo";

const install = {
  id: "chi_1",
  team_id: "T1",
  team_name: "Acme",
  bot_user_id: "U9",
  installer: "U1",
  members: ["U2", "U3"],
  anyone: false,
  created_at: 1_762_000_000_000,
};

describe("parseIntegrations", () => {
  it("reads what the server sends", () => {
    const got = parseIntegrations({
      slack: { configured: true, installs: [install] },
      discord: { configured: false, installs: [] },
      github: {
        configured: true,
        app_slug: "roll-code",
        install_url: "https://github.com/apps/roll-code/installations/new",
        repos: [{ repo: "acme/app", github: "acme/app", url: "https://github.com/acme/app", connected: true, mirror: "ok" }],
      },
      sealing: true,
    });
    expect(got.slack).toEqual({ configured: true, installs: [install] });
    expect(got.github.app_slug).toBe("roll-code");
    expect(got.github.repos).toEqual([{ repo: "acme/app", github: "acme/app", url: "https://github.com/acme/app", connected: true, mirror: "ok" }]);
    expect(got.sealing).toBe(true);
  });

  it("turns nothing, or the wrong shape, into nothing configured", () => {
    for (const v of [undefined, null, 42, "x", [], {}, { slack: [], discord: "no", github: 1, sealing: "yes" }]) {
      expect(parseIntegrations(v)).toEqual(NO_INTEGRATIONS);
    }
  });

  it("gives bad or missing fields their defaults and drops records it can't name", () => {
    const got = parseIntegrations({
      slack: {
        configured: "true",
        installs: [
          { id: "chi_2", team_id: "T2", members: ["U5", 7, "", "U5", null], anyone: "yes", created_at: "soon" },
          { team_name: "no id" },
          "chi_3",
        ],
      },
      github: {
        configured: true,
        app_slug: "",
        install_url: "javascript:alert(1)",
        repos: [{ repo: "acme/a", url: "data:text/html,x", mirror: "sideways" }, { github: "no roll repo" }],
      },
    });
    expect(got.slack.configured).toBe(false);
    expect(got.slack.installs).toEqual([
      { id: "chi_2", team_id: "T2", team_name: "T2", bot_user_id: "", installer: "", members: ["U5"], anyone: false, created_at: 0 },
    ]);
    expect(got.github).toEqual({
      configured: true,
      app_slug: null,
      install_url: null,
      repos: [{ repo: "acme/a", github: "", url: null, connected: false, mirror: "pending" }],
    });
    expect(got.discord).toEqual({ configured: false, installs: [] });
    expect(got.sealing).toBe(false);
  });

  it("only follows http(s) links", () => {
    expect(safeUrl("https://slack.com/oauth/v2/authorize?x=1")).toBe("https://slack.com/oauth/v2/authorize?x=1");
    expect(safeUrl("http://ghe.local/apps/x")).toBe("http://ghe.local/apps/x");
    for (const v of ["javascript:alert(1)", "/relative", "", null, 3]) expect(safeUrl(v)).toBeNull();
  });
});

describe("parseIntegrationsLink", () => {
  it("is nothing unless it asks for the integrations", () => {
    expect(parseIntegrationsLink(new URLSearchParams("owner=acme&connected=Acme"))).toBeNull();
    expect(parseIntegrationsLink({ settings: "owner" })).toBeNull();
  });

  it("reads a connection from the search params of either kind", () => {
    const want = { platform: "slack", connected: "Acme Inc", error: null };
    expect(parseIntegrationsLink(new URLSearchParams("owner=acme&settings=integrations&integration=slack&connected=Acme%20Inc"))).toEqual(want);
    expect(parseIntegrationsLink({ owner: "acme", settings: ["integrations"], integration: "slack", connected: "Acme Inc" })).toEqual(want);
  });

  it("explains known errors, keeps others, and an error wins over connected", () => {
    expect(parseIntegrationsLink(new URLSearchParams("settings=integrations&integration=discord&error=expired"))).toEqual({
      platform: "discord",
      connected: null,
      error: "The install link expired. Connect again.",
    });
    expect(parseIntegrationsLink({ settings: "integrations", integration: "slack", error: "Acme is connected to another owner", connected: "Acme" })).toEqual({
      platform: "slack",
      connected: null,
      error: "Acme is connected to another owner",
    });
  });

  it("ignores an unknown platform and bounds what it shows", () => {
    const got = parseIntegrationsLink({ settings: "integrations", integration: "irc", connected: "x".repeat(500) })!;
    expect(got.platform).toBeNull();
    expect(got.connected).toHaveLength(200);
  });
});

describe("createIntegrationsApi", () => {
  it("calls the owner's routes and parses the answers", async () => {
    const call = vi.fn(async (method: string, path: string): Promise<unknown> => {
      if (method === "GET") return { slack: { configured: true, installs: [install] } };
      if (path.endsWith("/install")) return { url: "https://slack.com/oauth/v2/authorize?state=s" };
      if (method === "PATCH") return { ...install, anyone: true };
      return null;
    });
    const api = createIntegrationsApi(call);
    expect((await api.list("ac me")).slack.installs).toHaveLength(1);
    expect(await api.install("acme", "slack")).toEqual({ url: "https://slack.com/oauth/v2/authorize?state=s" });
    expect((await api.update("acme", "slack", "chi_1", { anyone: true })).anyone).toBe(true);
    await api.remove("acme", "discord", "chi_1");
    expect(call.mock.calls).toEqual([
      ["GET", "/v1/code/agents/ac%20me/integrations"],
      ["POST", "/v1/code/agents/acme/integrations/slack/install"],
      ["PATCH", "/v1/code/agents/acme/integrations/slack/chi_1", { anyone: true }],
      ["DELETE", "/v1/code/agents/acme/integrations/discord/chi_1"],
    ]);
  });

  it("refuses an install link that isn't http(s)", async () => {
    const api = createIntegrationsApi(async () => ({ url: "javascript:alert(1)" }));
    await expect(api.install("acme", "slack")).rejects.toThrow(/no install link/);
  });
});

describe("the demo's integrations", () => {
  it("has Slack connected to Acme, Discord unconfigured and two GitHub repos, and keeps edits", async () => {
    const api = demoIntegrations();
    const d = await api.list("demo");
    expect(d.slack.installs.map((i) => i.team_name)).toEqual(["Acme"]);
    expect(d.discord.configured).toBe(false);
    expect(d.github.repos).toHaveLength(2);
    await api.update("demo", "slack", "chi_demo1", { members: ["U03GRACE", "U04LIN"] });
    expect((await api.list("demo")).slack.installs[0].members).toEqual(["U03GRACE", "U04LIN"]);
    await expect(api.update("demo", "slack", "chi_demo1", { members: ["not an id"] })).rejects.toThrow(/letters and digits/);
    await expect(api.install("demo", "slack")).rejects.toThrow(/demo can't connect Slack/);
    await api.remove("demo", "slack", "chi_demo1");
    expect((await api.list("demo")).slack.installs).toEqual([]);
  });
});
