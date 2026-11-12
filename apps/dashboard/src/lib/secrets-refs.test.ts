import { describe, expect, it } from "vitest";
import { accept, check, display, refs, suggest, typing, type Listing } from "./secrets-refs";

describe("secret references", () => {
  it("parse as the server does (refs.rs's test), with $${ escaping", () => {
    const r = refs("postgres://${USER}:${production.db.PASS}@${staging.HOST}/x ${not a ref} $${LITERAL}");
    expect(r.map(display)).toEqual(["USER", "production.db.PASS", "staging.HOST"]);
    expect(r[1].path).toBe("/db");
  });

  it("complete keys here, then environments, then a folder's keys and subfolders", () => {
    const folders: Record<string, Listing> = { "staging:/": { keys: ["DB_URL"], folders: ["db"] } };
    const look = (e: string, p: string) => folders[`${e}:${p}`];
    expect(suggest("d", ["DEBUG", "API_KEY"], ["dev", "staging"], look).items.map((s) => s.insert)).toEqual(["DEBUG", "dev."]);
    expect(suggest("staging.d", [], ["staging"], look).items.map((s) => s.insert)).toEqual(["staging.DB_URL", "staging.db."]);
    expect(suggest("staging.db.", [], ["staging"], look).need).toEqual({ env: "staging", path: "/db" });

    const v = "x=${sta";
    expect(typing(v, v.length)).toEqual({ start: 2, query: "sta" });
    expect(typing("$${sta", 6)).toBeNull();
    const env = accept(v, v.length, { insert: "staging.", kind: "env", label: "staging." });
    expect(env).toEqual({ value: "x=${staging.", caret: 12 });
    expect(accept(env.value, env.caret, { insert: "staging.DB_URL", kind: "key", label: "DB_URL" })).toEqual({ value: "x=${staging.DB_URL}", caret: 19 });

    const c = check("${MISSING} ${staging.NOPE} ${a b}", "SELF", new Set(["HERE"]), look);
    expect(c.warnings).toHaveLength(3);
  });
});
