import { readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { validOwnerName } from "./code-api";
import { RESERVED_NAMES } from "./reserved-names";

describe("reserved names", () => {
  it("are lowercase and listed once", () => {
    expect(RESERVED_NAMES.every((n) => n === n.toLowerCase())).toBe(true);
    expect(new Set(RESERVED_NAMES).size).toBe(RESERVED_NAMES.length);
  });

  it("cover every top-level route of the app", () => {
    const app = join(__dirname, "..", "app");
    const routes = readdirSync(app).filter((name) => {
      if (statSync(join(app, name)).isDirectory()) return !/^[[(_@]/.test(name);
      return /^(icon|apple-icon|favicon|opengraph-image|twitter-image|robots|sitemap|manifest)\./.test(name);
    });
    for (const r of routes) expect(RESERVED_NAMES, r).toContain(r);
  });

  it("include the names a valid owner could otherwise take", () => {
    // Names that pass owner validation must be blocked by the list itself.
    for (const n of ["dashboard", "settings", "api", "new", "login"]) {
      expect(validOwnerName(n)).toBe(true);
      expect(RESERVED_NAMES).toContain(n);
    }
  });
});
