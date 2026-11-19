import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";

// A signed-in person reaches rolld by their session cookie (and CSRF header), not the operator
// token: a raw `fetch` to rolld without `withSession` answers them 401 (an export, a log, a QR
// code). `call` and `streamSse` add it; any other fetch to rolld must too.
describe("fetches to rolld", () => {
  it("carry the session (withSession)", () => {
    const dir = import.meta.dirname;
    const bare: string[] = [];
    for (const f of readdirSync(dir).filter((f) => f.endsWith(".ts") && !f.endsWith(".test.ts"))) {
      const src = readFileSync(join(dir, f), "utf8");
      for (const m of src.matchAll(/fetch\(\s*`\$\{(ROLL_API|TASKS_API)\}/g)) {
        if (!/withSession\(/.test(src.slice(m.index, m.index + 400))) bare.push(`${f}:${src.slice(0, m.index).split("\n").length}`);
      }
    }
    expect(bare).toEqual([]);
  });
});
