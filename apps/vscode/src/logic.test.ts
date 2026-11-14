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
  it("give each entry's value and where it is, quotes and comments aside", () => {
    expect(isDotenvFile("/w/app/.env.local") && isDotenvFile(".env") && !isDotenvFile("/w/env.ts")).toBe(true);
    const lines = dotenvLines(`# db\nexport DATABASE_URL="postgres://u:p@h/db"\nSTRIPE_KEY=sk_test_1 # test key\nREF=roll://staging/STRIPE_KEY\n`);
    expect(lines.map((l) => [l.line, l.key, l.value, l.reference])).toEqual([
      [1, "DATABASE_URL", "postgres://u:p@h/db", false],
      [2, "STRIPE_KEY", "sk_test_1", false],
      [3, "REF", "roll://staging/STRIPE_KEY", true],
    ]);
    const db = lines[0];
    expect(`export DATABASE_URL="postgres://u:p@h/db"`.slice(db.valueStart, db.valueEnd)).toBe(db.value);
    expect(dotenvLines(`PEM="a\\nb \\"c\\""`)[0].value).toBe('a\nb "c"');
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
