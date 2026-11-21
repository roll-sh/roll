import { describe, expect, it } from "vitest";
import { parseOrigins, quickstart } from "./search-browser";

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
});
