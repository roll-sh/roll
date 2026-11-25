import { describe, expect, it } from "vitest";
import type { BrowserKeyUsage } from "./search-api";
import { parseOrigins, quickstart, usageSeries } from "./search-browser";

describe("browser search", () => {
  it("reads allowed origins as the cluster keeps them, refusing what it would", () => {
    const r = parseOrigins("HTTPS://Shop.Example.com:443/\nhttp://localhost:3000\n\n/https://.*\\.preview\\.example/\n*\nhttp://localhost:3000");
    expect(r.errors).toEqual([]);
    expect(r.origins).toEqual(["https://shop.example.com", "http://localhost:3000", "/https://.*\\.preview\\.example/", "*"]);
    for (const bad of ["shop.example.com", "ftp://x", "https://x/path", "https://x:0", "/(/"]) {
      expect(parseOrigins(bad).errors, bad).toHaveLength(1);
    }
  });

  it("fills the quickstart in with the cluster's endpoint and the new key", () => {
    const url = "https://shop.search.example.com";
    const app = quickstart("search-application", url, "shop-search", "a2V5");
    expect(app).toContain('SearchApplicationClient("shop-search", "https://shop.search.example.com", "a2V5")');
    expect(quickstart("instantsearch", url, "products")).toContain('apiKey: "<browser key>"');
    expect(quickstart("search-ui", url, "products", "a2V5")).toContain('index: "products"');
  });

  it("lays a key's usage out a step a value, refusals by reason, most first", () => {
    const u: BrowserKeyUsage = {
      key: "k1",
      window: "24h",
      step_s: 900,
      from: 9000,
      to: 9000 + 4 * 900,
      points: [
        { at: 9000, requests: 5, throttled: 0, refused: 0, reasons: {} },
        { at: 9000 + 2 * 900, requests: 9, throttled: 4, refused: 1, reasons: { key_rate: 3, user_rate: 1, origin: 1 } },
        { at: 99_999, requests: 1, throttled: 0, refused: 0, reasons: {} },
      ],
      totals: { requests: 14, throttled: 4, refused: 1, reasons: { key_rate: 3, user_rate: 1, origin: 1, policy: 0 }, users: [], origins: [] },
    };
    const s = usageSeries(u);
    expect(s.requests).toEqual([5, 0, 9, 0]);
    expect(s.throttled).toEqual([0, 0, 4, 0]);
    expect(s.reasons.map((r) => [r.reason, r.count, r.throttles])).toEqual([
      ["key_rate", 3, true],
      ["user_rate", 1, true],
      ["origin", 1, false],
    ]);
  });
});
