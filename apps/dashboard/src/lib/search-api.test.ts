import { describe, expect, it } from "vitest";
import { perIndexRoute } from "./search-api";

describe("perIndexRoute", () => {
  it("maps an index's requests onto its per-index routes, with the query string", () => {
    expect(perIndexRoute("POST", "/logs/_search")).toEqual({ method: "POST", path: "/indices/logs/search" });
    expect(perIndexRoute("GET", "/logs/_mapping")).toEqual({ method: "GET", path: "/indices/logs/mapping" });
    expect(perIndexRoute("PUT", "/logs/_doc/a%2Fb?refresh=wait_for")).toEqual({ method: "PUT", path: "/indices/logs/docs/a%2Fb?refresh=wait_for" });
    expect(perIndexRoute("POST", "/logs/_doc?refresh=wait_for")).toEqual({ method: "POST", path: "/indices/logs/docs?refresh=wait_for" });
    expect(perIndexRoute("DELETE", "/logs")).toEqual({ method: "DELETE", path: "/indices/logs" });
    expect(perIndexRoute("GET", "/logs/_analyze")).toEqual({ method: "POST", path: "/indices/logs/analyze" });
  });

  it("leaves cluster APIs and anything else to the console", () => {
    for (const [m, p] of [["GET", "/_cat/indices"], ["POST", "/_bulk"], ["GET", "/logs/_stats"], ["POST", "/logs/_doc/1/_update"], ["GET", "/"]]) {
      expect(perIndexRoute(m, p)).toBeNull();
    }
  });
});
