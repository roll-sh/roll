import { describe, expect, it } from "vitest";
import { catalogFor, denyRedirect, draftOf, expiringSoon, mcpConfig, parseAuthorize, presetFor, presetRules, rulesOf, statusOf, type Preset } from "./access-api";

const presets: Preset[] = [
  { id: "read-only", name: "Read only", description: "", rules: [{ product: "*", resources: "*", actions: { read: "allow" } }] },
  { id: "agent", name: "Agent", description: "", rules: [{ product: "*", resources: "*", actions: { read: "allow", delete: "ask" } }] },
  { id: "project", name: "Project", description: "", rules: [{ product: "*", resources: "project", actions: { write: "allow" } }] },
];

describe("access", () => {
  it("connects clients with OAuth unless a token is given, and never puts a token in an OAuth config", () => {
    const url = "https://api.roll.sh/v1/mcp";
    expect(mcpConfig("claude-code", url).text).toBe(`claude mcp add --transport http roll ${url}`);
    expect(mcpConfig("cursor", url).text).not.toContain("Authorization");
    expect(mcpConfig("claude-code", url, "rlt_x").text).toContain('--header "Authorization: Bearer rlt_x"');
    expect(JSON.parse(mcpConfig("vscode", url, "rlt_x").text).servers.roll).toEqual({ type: "http", url, headers: { Authorization: "Bearer rlt_x" } });
  });

  it("reads the consent request, starts from the asked preset, and fills in the project", () => {
    const ok = parseAuthorize({ client_id: "cli_1", redirect_uri: "http://127.0.0.1:5000/cb", code_challenge: "x".repeat(43), response_type: "code", scope: "read-only" });
    expect("error" in ok).toBe(false);
    expect(parseAuthorize({ client_id: "c", redirect_uri: "r", response_type: "code", code_challenge: "x", code_challenge_method: "plain" })).toHaveProperty("error");
    expect(presetFor("read-only", presets)?.id).toBe("read-only");
    expect(presetFor(undefined, presets)?.id).toBe("agent");
    expect(presetFor("full", [...presets, { id: "full", name: "Full", description: "", rules: [] }])?.id).toBe("agent");
    expect(presetRules(presets[2], "prj_a")[0].resources).toEqual([{ project: "prj_a" }]);
    expect(presetRules(presets[2])[0].resources).toBe("*");
    expect(denyRedirect("http://127.0.0.1:5000/cb?x=1", "s")).toBe("http://127.0.0.1:5000/cb?x=1&error=access_denied&error_description=The+user+denied+the+request&state=s");
    expect(statusOf({ revoked_at: null, expires_at: 1 }, 2)).toBe("expired");
  });
});

describe("tokens", () => {
  it("names the live tokens that expire within a week, soonest first", () => {
    const day = 86_400_000;
    const g = (name: string, expires_at: number | null, revoked_at: number | null = null) => ({ name, expires_at, revoked_at });
    const now = 100 * day;
    const list = [g("later", now + 30 * day), g("soon", now + 3 * day), g("sooner", now + day), g("gone", now - day), g("never", null), g("revoked", now + day, now)];
    expect(expiringSoon(list, now).map((x) => x.name)).toEqual(["sooner", "soon"]);
  });
});

describe("the builder", () => {
  it("offers a signed-in user only the products their account reaches", () => {
    const c = { products: ["workspace", "postgres", "code", "agents"], actions: [], presets, routes: [], operator_only: [], api_url: "" };
    const user = { kind: "user" as const, user: { id: "usr_a", handle: "a" }, reach: ["code", "agents", "workspace"], locked: true };
    expect(catalogFor(c, user).products).toEqual(["workspace", "code", "agents"]);
    expect(catalogFor(c, { kind: "operator", locked: true }).products).toEqual(c.products);
  });

  it("round-trips rules through its matrix, and leaves rules it can't show as they are", () => {
    const rules = [
      { product: "postgres", resources: [{ project: "prj_a" }], environments: ["development"], actions: { read: "allow", delete: "ask" } },
      { product: "redis", resources: [{ project: "prj_a" }], environments: ["development"], actions: { read: "allow" } },
    ] as const;
    const d = draftOf(rules as never)!;
    expect(d.project).toBe("prj_a");
    expect(rulesOf(d)).toEqual(rules);
    expect(draftOf([{ product: "*", resources: "*", actions: {} }, { product: "x", resources: [{ id: "a" }], actions: {} }])).toBeNull();
  });
});
