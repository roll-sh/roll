import { describe, expect, it } from "vitest";
import { complete, completions, openRef } from "./var-refs";

describe("completing a variable reference", () => {
  const c = { own: ["API_KEY"], services: [{ name: "db", vars: ["PASS"] }, { name: "api", vars: [] }] };
  it("offers services, then their variables and roll's own, and finishes the reference", () => {
    expect(openRef("postgres://app:${{ d")).toEqual({ start: 15, partial: "d" });
    expect(openRef("plain text")).toBeNull();
    expect(completions("d", c)).toEqual(["db."]);
    expect(completions("db.P", c)).toEqual(["db.PASS", "db.PORT"]);
    expect(completions("A", c)).toEqual(["api.", "API_KEY"]);
    expect(complete("postgres://app:${{ db.P", "db.PASS")).toBe("postgres://app:${{ db.PASS }}");
    expect(complete("${{ d", "db.")).toBe("${{ db.");
  });
});
