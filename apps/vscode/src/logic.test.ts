import { describe, expect, it } from "vitest";
import { completesNames, dotenvLines, envReads, isDotenvFile, nearestSetup, problems, rollRef, type GridLike } from "./logic.ts";

const grid: GridLike = {
  environments: ["development", "staging", "production"],
  rows: [
    { key: "STRIPE_KEY", cells: { development: {}, staging: {}, production: null } },
    { key: "DATABASE_URL", cells: { development: {}, staging: {}, production: {} } },
  ],
};

describe("the code's environment reads", () => {
  it("are found in each language, with the name's own offsets", () => {
    const js = `const k = process.env.STRIPE_KEY ?? process.env["DATABASE_URL"];`;
    expect(envReads(js, "typescript").map((r) => [r.name, js.slice(r.start, r.end)])).toEqual([
      ["STRIPE_KEY", "STRIPE_KEY"],
      ["DATABASE_URL", "DATABASE_URL"],
    ]);
    // The name, not an earlier word that spells it (`environ` in `os.environ`).
    const py = `os.environ.get("environ")`;
    const [r] = envReads(py, "python");
    expect([r.name, py.slice(r.start, r.end), r.start]).toEqual(["environ", "environ", 16]);
    expect(envReads(`std::env::var("PORT")`, "rust").map((r) => r.name)).toEqual(["PORT"]);
    expect(envReads(`os.Getenv("REGION")`, "go").map((r) => r.name)).toEqual(["REGION"]);
    expect(envReads(`process.env.X`, "markdown")).toEqual([]);
  });

  it("warn only where roll sets a key elsewhere but not in the workspace's environment", () => {
    const text = `process.env.STRIPE_KEY; process.env.DATABASE_URL; process.env.NODE_ENV`;
    expect(problems(envReads(text, "javascript"), grid, "production").map((p) => p.message)).toEqual(["STRIPE_KEY isn't set in production (set in development, staging)"]);
    expect(problems(envReads(text, "javascript"), grid, "staging")).toEqual([]);
  });

  it("complete names where a name goes", () => {
    expect(completesNames("const k = process.env.ST", "typescript")).toBe(true);
    expect(completesNames(`os.getenv("ST`, "python")).toBe(true);
    expect(completesNames("const process = env.", "typescript")).toBe(false);
  });
});

describe("dotenv files", () => {
  it("are read as the CLI reads them, and each entry's range covers all of it", () => {
    expect(isDotenvFile("/w/app/.env.local") && isDotenvFile(".env") && !isDotenvFile("/w/env.ts")).toBe(true);
    const text = [
      "# db",
      'export DATABASE_URL="postgres://u:p@h/db" # the main one',
      "STRIPE_KEY=sk_test_1 # test key",
      "TABBED=a\t#b",
      'PEM="-----BEGIN KEY-----',
      "MIIBVgIBADAN",
      '-----END KEY-----"',
      "REF=roll://staging/STRIPE_KEY",
      "ESC=\"a\\nb \\\"c\\\"\"",
    ].join("\n");
    const lines = dotenvLines(text);
    expect(lines.map((l) => [l.line, l.endLine, l.key, l.value, l.reference])).toEqual([
      [1, 1, "DATABASE_URL", "postgres://u:p@h/db", false],
      [2, 2, "STRIPE_KEY", "sk_test_1", false],
      [3, 3, "TABBED", "a\t#b", false],
      [4, 6, "PEM", "-----BEGIN KEY-----\nMIIBVgIBADAN\n-----END KEY-----", false],
      [7, 7, "REF", "roll://staging/STRIPE_KEY", true],
      [8, 8, "ESC", 'a\nb "c"', false],
    ]);
    // Replacing an entry's range replaces all of it: the quotes, every line of a multi-line one.
    const rows = text.split("\n");
    const pem = lines[3];
    const replaced = [...rows.slice(0, pem.line), rows[pem.line].slice(0, pem.from) + "roll://x/PEM" + rows[pem.endLine].slice(pem.to), ...rows.slice(pem.endLine + 1)];
    expect(replaced.slice(4, 6)).toEqual(["PEM=roll://x/PEM", "REF=roll://staging/STRIPE_KEY"]);
    const db = lines[0];
    expect(rows[1].slice(db.from, db.to)).toBe('"postgres://u:p@h/db"');
    expect(rollRef("staging", "/payments", "STRIPE_KEY")).toBe("roll://staging/payments/STRIPE_KEY");
  });
});

describe("the CLI's setup", () => {
  it("applies to the directory it was made in and the ones under it", () => {
    const all = { "/home/a/shop": { project: "prj_1", name: "shop", env: "staging", path: "/" } };
    expect(nearestSetup(all, "/home/a/shop/web/src")?.env).toBe("staging");
    expect(nearestSetup(all, "/home/a/other")).toBeUndefined();
  });
});
