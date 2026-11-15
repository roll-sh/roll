import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { gzip, skipped, tarBundle } from "./bundle";

describe("a dropped folder as roll up bundles it", () => {
  it("is a gzipped tar GNU tar reads: the bundle's entries first, directories before their files, long names kept", async () => {
    const enc = new TextEncoder();
    const long = `assets/${"a".repeat(120)}.js`;
    const tar = tarBundle([
      { path: "index.html", data: enc.encode("<h1>hi</h1>") },
      { path: long, data: enc.encode("x") },
    ]);
    const dir = mkdtempSync(join(tmpdir(), "bundle-"));
    const file = join(dir, "b.tgz");
    writeFileSync(file, new Uint8Array(await (await gzip(tar)).arrayBuffer()));
    const names = execFileSync("tar", ["-tzf", file], { encoding: "utf8" }).trim().split("\n");
    expect(names).toEqual(["roll-bundle-1", "env", "tree/assets", `tree/${long}`, "tree/index.html"]);
    expect(execFileSync("tar", ["-xOzf", file, "tree/index.html"], { encoding: "utf8" })).toBe("<h1>hi</h1>");
    expect([skipped("node_modules/x/index.js"), skipped(".env.local"), skipped(".env.example"), skipped("src/app.js")]).toEqual([true, true, false, false]);
  });
});
