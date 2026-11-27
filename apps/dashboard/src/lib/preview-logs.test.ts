import { describe, expect, it } from "vitest";
import { logFilter } from "./preview-logs";

describe("a preview's log lines", () => {
  it("are filtered by text case-blind or by /regex/, and a broken regex is said", () => {
    expect(["GET /checkout 500", "GET / 200"].filter(logFilter("checkout")!)).toEqual(["GET /checkout 500"]);
    expect(["GET /checkout 500", "GET / 200"].filter(logFilter("/ 5\\d\\d$/")!)).toEqual(["GET /checkout 500"]);
    expect(["Card DECLINED"].filter(logFilter("/declined/i")!)).toEqual(["Card DECLINED"]);
    expect(logFilter("/(/")).toBeNull();
    expect(["a", "b"].filter(logFilter("  ")!)).toEqual(["a", "b"]);
  });
});
