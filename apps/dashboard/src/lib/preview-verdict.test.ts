import { describe, expect, it } from "vitest";
import type { Preview } from "./previews-api";
import { verdict } from "./preview-verdict";
import { parseRouteBudgets } from "./previews-api";

const base = { services: [], schema_risks: [], base_change: null, visual_review: null, page_health: null, page_links: null, page_a11y: null } as unknown as Preview;

describe("verdict", () => {
  it("leads with what breaks, then what waits on someone, then what's worth a look; nothing when nothing's new", () => {
    expect(verdict(base)).toEqual([]);
    const p = {
      ...base,
      services: [{ name: "web", state: "failed" }, { name: "wiki", state: "sleeping" }],
      schema_risks: ["users.email becomes NOT NULL"],
      base_change: { branch: "main", percent: 0.3, phone_percent: null, api_breaking: ["GET /users: limit becomes required"], errors_added: 1, links_added: 1, endpoints: [{ path: "/a", ms: 90, status: 200, main_ms: 30 }] },
      new_errors: { branch: "main", total: 2, errors: [{ service: "web", title: "TypeError" }] },
      slower_routes: { branch: "main", routes: [{ name: "GET /a", count: 9, p50_ms: 400, main_p50_ms: 100 }] },
      visual_review: { state: "changed", branch: "main", percent: 0.3, by: null, at: null, required: true, head: "h", pages: [{ path: "", percent: 0.2, by: null, at: null }, { path: "/p", percent: 0.3, by: "ada", at: 1, unstable: true }] },
    } as unknown as Preview;
    expect(verdict(p).map((v) => [v.target, v.text, v.tone])).toEqual([
      ["services", "1 service failed", "danger"],
      ["api", "1 breaking API change", "danger"],
      ["schema", "1 migration risk", "danger"],
      ["checks", "2 new problems against main", "danger"],
      ["errors", "2 new errors against main", "danger"],
      ["visual", "1 page differs from main: review before it lands", "attention"],
      ["endpoints", "1 endpoint slower than main", "note"],
      ["routes", "1 route slower than main", "note"],
      ["visual", "1 page moves by itself", "note"],
    ]);
  });

  it("says a route over its budget as a danger", () => {
    const p = { ...base, slower_routes: { branch: "main", routes: [{ name: "GET /new", count: 9, p50_ms: 400, main_p50_ms: 0, budget_ms: 300 }] } } as unknown as Preview;
    expect(verdict(p).map((v) => [v.target, v.text, v.tone])).toEqual([["routes", "1 route slower than main or over budget", "danger"]]);
    // The rules dialog's lines: a route (spaces and numbers in it too) then its milliseconds.
    expect(parseRouteBudgets("GET /items/2 300\nPOST /pay 800ms\nGET /x: fast")).toEqual({ routes: { "GET /items/2": 300, "POST /pay": 800 }, bad: ["GET /x: fast"] });
  });
});
