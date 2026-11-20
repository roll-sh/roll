import { describe, expect, it } from "vitest";
import { movedPanel } from "./secrets-tabs";

describe("a link to a panel that moved off the Syncs tab", () => {
  it("lands on the panel's new tab", () => {
    expect(movedPanel("syncs", "#transit-keys")).toBe("keys");
    expect(movedPanel("syncs", "kubernetes")).toBe("credentials");
    expect(movedPanel("syncs", "#syncs")).toBeNull();
    expect(movedPanel("syncs", "")).toBeNull();
  });
});
