import { describe, expect, it, vi } from "vitest";
import { decode, DecodeError } from "./decode";
import { createLinearApi, DISCONNECTED, linearReturn, linearStatusD, memoryLinear, type LinearStatus } from "./linear-api";

/** A connected status as the server writes it (`status_json`). */
const FULL = {
  connected: true,
  app: { oauth: true, webhook: true, webhook_url: "https://api.roll.dev/v1/code/linear/webhook", redirect_uri: "https://api.roll.dev/v1/code/linear/oauth/callback", secret_key: true },
  connection: {
    kind: "api_key",
    workspace: { id: "org_1", name: "Acme", url_key: "acme", url: "https://linear.app/acme" },
    user: { id: "usr_1", name: "Ada" },
    agent: false,
    scopes: ["read", "write"],
    status: "reauth",
    error: "Linear refused the token",
    expires_at: null,
    created_at: 1_762_000_000_000,
    created_by: "acme",
    settings: { repos: { ENG: "roll", WEB: "acme/web" }, default_repo: "roll" },
    webhook: {
      mode: "manual",
      url: "https://api.roll.dev/v1/code/linear/webhook/lwh_1",
      status: "failing",
      secret_hint: "f00d",
      last_delivery_at: 1_762_000_100_000,
      rejected_at: 1_762_000_200_000,
      rejected_why: "a bad signature",
    },
  },
  deliveries: [
    { id: "ldl_2", at: 1_762_000_100_000, delivery: "d2", type: "Comment", action: "create", summary: "ENG-1: Ada commented", result: "recorded", events: 1 },
    { id: "ldl_1", at: 1_762_000_000_000, delivery: "d1", type: "Issue", action: "update", summary: "ENG-1 moved", result: "ignored", events: 0 },
  ],
};

describe("LinearStatus", () => {
  it("decodes a connected status strictly, keeping every field", () => {
    expect(decode(linearStatusD, FULL, { strict: true })).toEqual(FULL);
  });

  it("decodes a disconnected one (settings default when a newer server leaves them out)", () => {
    const s = decode(linearStatusD, { connected: false, app: { ...FULL.app, oauth: false, redirect_uri: null }, connection: null, deliveries: [] }, { strict: true });
    expect(s).toEqual({ connected: false, app: { ...FULL.app, oauth: false, redirect_uri: null }, connection: null, deliveries: [] });
    const { settings: _, ...noSettings } = FULL.connection;
    expect(decode(linearStatusD, { ...FULL, connection: noSettings }).connection?.settings).toEqual({ repos: {} });
  });

  it("rejects junk", () => {
    for (const junk of [null, "ok", [], {}, { ...FULL, connected: "yes" }, { ...FULL, connection: { ...FULL.connection, kind: "password" } }, { ...FULL, deliveries: [{ id: 1 }] }]) {
      expect(() => decode(linearStatusD, junk)).toThrow(DecodeError);
    }
    // Strict decoding catches a field added on the server and not here.
    expect(() => decode(linearStatusD, { ...FULL, extra: 1 }, { strict: true })).toThrow(/unknown key/);
  });
});

describe("the Linear client", () => {
  it("calls the owner routes", async () => {
    const call = vi.fn(async (method: string, path: string): Promise<unknown> => {
      if (path.endsWith("/oauth")) return { url: "https://linear.app/oauth/authorize?state=s" };
      if (path.endsWith("/teams")) return { teams: [{ id: "t1", key: "ENG", name: "Engineering" }] };
      if (method === "DELETE") return null;
      return FULL;
    });
    const api = createLinearApi(call);
    expect((await api.getLinear("acme o")).connection?.workspace.name).toBe("Acme");
    expect(call).toHaveBeenLastCalledWith("GET", "/v1/code/agents/acme%20o/linear");
    expect(await api.connectOAuth("acme", "/dashboard/agents#agents/settings/owner")).toBe("https://linear.app/oauth/authorize?state=s");
    expect(call).toHaveBeenLastCalledWith("POST", "/v1/code/agents/acme/linear/oauth", { return_to: "/dashboard/agents#agents/settings/owner" });
    await api.putKey("acme", "lin_api_x");
    expect(call).toHaveBeenLastCalledWith("PUT", "/v1/code/agents/acme/linear/key", { key: "lin_api_x" });
    await api.putSettings("acme", { repos: { ENG: "roll" } });
    expect(call).toHaveBeenLastCalledWith("PUT", "/v1/code/agents/acme/linear/settings", { repos: { ENG: "roll" } });
    await api.putWebhookSecret("acme", "s3cretsecret");
    expect(call).toHaveBeenLastCalledWith("PUT", "/v1/code/agents/acme/linear/webhook/secret", { secret: "s3cretsecret" });
    expect(await api.getTeams("acme")).toEqual([{ id: "t1", key: "ENG", name: "Engineering" }]);
    await api.disconnect("acme");
    expect(call).toHaveBeenLastCalledWith("DELETE", "/v1/code/agents/acme/linear");
  });

  it("in memory: a key connects with a hand-made webhook until its secret comes", async () => {
    const api = memoryLinear(DISCONNECTED);
    await expect(api.putKey("acme", "nope")).rejects.toMatchObject({ status: 400 });
    const s: LinearStatus = await api.putKey("acme", "lin_api_0123456789");
    expect(s.connection).toMatchObject({ kind: "api_key", webhook: { mode: "manual", status: "needs_secret" } });
    expect((await api.putWebhookSecret("acme", "whsec_abcdef12")).connection?.webhook).toMatchObject({ status: "waiting", secret_hint: "ef12" });
    await api.disconnect("acme");
    expect((await api.getLinear("acme")).connected).toBe(false);
  });
});

describe("back from Linear's OAuth", () => {
  it("reads the answer after the agents hash (where the server appends it) and cleans the URL", () => {
    expect(linearReturn("https://roll.dev/dashboard/agents#agents/settings/owner?linear=connected")).toEqual({
      result: { outcome: "connected" },
      clean: "/dashboard/agents#agents/settings/owner",
    });
    expect(linearReturn("https://roll.dev/dashboard/agents?demo#agents/settings/owner&linear=error&reason=Linear%3A%20access_denied")).toEqual({
      result: { outcome: "error", reason: "Linear: access_denied" },
      clean: "/dashboard/agents?demo#agents/settings/owner",
    });
    expect(linearReturn("https://roll.dev/dashboard?linear=error")).toEqual({ result: { outcome: "error", reason: "Linear didn't connect." }, clean: "/dashboard" });
    expect(linearReturn("https://roll.dev/dashboard/agents#agents/settings/owner").result).toBeNull();
  });
});
