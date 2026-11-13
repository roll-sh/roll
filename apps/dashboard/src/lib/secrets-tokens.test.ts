import { describe, expect, it } from "vitest";
import type { Grant } from "./access-api";
import { grantFor, tokenOf } from "./secrets-tokens";

describe("a project's tokens as roll-access grants", () => {
  it("makes one rule on the project's secrets, and reads it back", () => {
    const g = grantFor("prj_a", { name: "ci", environments: ["production"], paths: ["/api"], actions: ["read", "reveal"], ttl_days: 2, allowed_ips: ["203.0.113.0/24"] }, 1_000);
    expect(g).toEqual({
      name: "ci",
      rules: [{ product: "secrets", resources: [{ project: "prj_a" }], environments: ["production"], paths: ["/api"], actions: { read: "allow", reveal: "allow" } }],
      expires_at: 1_000 + 2 * 86_400_000,
      ip_allow: ["203.0.113.0/24"],
    });
    const grant: Grant = { id: "tok_1", name: "ci", kind: "token", rules: g.rules, expires_at: g.expires_at ?? null, ip_allow: g.ip_allow ?? [], created_by: "operator", created_at: 1, last_used_at: null, last_used_ip: null, revoked_at: null };
    expect(tokenOf(grant, "prj_a")).toMatchObject({ id: "tok_1", environments: ["production"], paths: ["/api"], actions: ["read", "reveal"], allowed_ips: ["203.0.113.0/24"] });
    // A rule on another project (or one only asking) reaches nothing here.
    expect(tokenOf({ ...grant, rules: [{ ...g.rules[0], resources: [{ project: "prj_b" }] }] }, "prj_a").actions).toEqual([]);
  });
});
