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
    // TanStack's file names: `foo.tsx`, `foo.bar.tsx` and `foo/` are all `/foo`; params (`$x`),
    // groups, pathless layouts, ignored files, the root and the home page aren't names.
    const dir = join(__dirname, "..", "routes");
    const routes = readdirSync(dir)
      .map((name) => (statSync(join(dir, name)).isDirectory() ? name : name.split(".")[0]))
      .filter((top) => !/^[$(_\-[]/.test(top) && top !== "index" && top !== "route");
    expect(routes).toContain("dashboard");
    for (const r of routes) expect(RESERVED_NAMES, r).toContain(r);
  });

  it("cover every static file at the top level", () => {
    for (const f of readdirSync(join(__dirname, "..", "..", "public"))) expect(RESERVED_NAMES, f).toContain(f);
  });

  it("include the names a valid owner could otherwise take", () => {
    // Names that pass owner validation must be blocked by the list itself.
    for (const n of ["dashboard", "settings", "api", "new", "login"]) {
      expect(validOwnerName(n)).toBe(true);
      expect(RESERVED_NAMES).toContain(n);
    }
  });
});
