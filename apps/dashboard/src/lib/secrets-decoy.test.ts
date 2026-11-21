import { describe, expect, it } from "vitest";
import type { Decoy } from "@roll/secrets/types";
import { decoyUse } from "./secrets-decoy";

const base: Decoy = { kind: "rlt", created_at: 1, created_by: "operator", uses: 0, used_at: null, used_ip: null, used_route: null, used_endpoint: null };
const when = () => "2m ago";

describe("decoyUse", () => {
  it("says where a decoy was last used: the endpoint or the route, and the address", () => {
    expect(decoyUse({ ...base, kind: "postgres", uses: 3, used_at: 5, used_ip: "203.0.113.7", used_endpoint: "ep-abc123" }, when)).toBe("used 3×, last from 203.0.113.7 on ep-abc123, 2m ago");
    expect(decoyUse({ ...base, uses: 1, used_at: 5, used_ip: "203.0.113.7", used_route: "/v1/secrets/whoami" }, when)).toBe("used once, last from 203.0.113.7 calling /v1/secrets/whoami, 2m ago");
    expect(decoyUse({ ...base, uses: 2, used_at: 5 }, when)).toBe("used 2×, last 2m ago");
  });
});
