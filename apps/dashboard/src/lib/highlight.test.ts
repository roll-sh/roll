import { describe, expect, it } from "vitest";
import { HIGHLIGHT_MAX_BYTES, highlight } from "./highlight";

describe("highlight", () => {
  it("skips unknown languages and huge files", async () => {
    expect(await highlight("x", null)).toBeNull();
    expect(await highlight("x".repeat(HIGHLIGHT_MAX_BYTES + 1), "typescript")).toBeNull();
    expect(await highlight("x", "not-a-language")).toBeNull();
  });
});
