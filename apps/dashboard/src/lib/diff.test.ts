import { describe, expect, it } from "vitest";
import { parsePatch, splitRows } from "./diff";

const patch = [
  "diff --git a/x b/x",
  "index 1..2 100644",
  "--- a/x",
  "+++ b/x",
  "@@ -1,4 +1,4 @@ fn main",
  " one",
  "-two",
  "+TWO",
  "+extra",
  " three",
  "\\ No newline at end of file",
  "@@ -10,2 +11,1 @@",
  "-gone",
  " kept",
  "",
].join("\n");

describe("parsePatch", () => {
  it("numbers old and new lines and drops file headers", () => {
    const rows = parsePatch(patch);
    expect(rows.map((r) => [r.kind, r.old, r.new, r.text])).toEqual([
      ["hunk", null, null, "@@ -1,4 +1,4 @@ fn main"],
      ["ctx", 1, 1, "one"],
      ["del", 2, null, "two"],
      ["add", null, 2, "TWO"],
      ["add", null, 3, "extra"],
      ["ctx", 3, 4, "three"],
      ["note", null, null, "\\ No newline at end of file"],
      ["hunk", null, null, "@@ -10,2 +11,1 @@"],
      ["del", 10, null, "gone"],
      ["ctx", 11, 11, "kept"],
    ]);
  });

  it("handles an empty patch", () => {
    expect(parsePatch("")).toEqual([]);
  });
});

describe("splitRows", () => {
  it("pairs deletions with the additions after them", () => {
    const rows = splitRows(parsePatch(patch));
    const pair = rows.map(([l, r]) => [l?.text ?? null, r?.text ?? null]);
    expect(pair.slice(1, 5)).toEqual([
      ["one", "one"],
      ["two", "TWO"],
      [null, "extra"],
      ["three", "three"],
    ]);
    expect(pair).toContainEqual(["gone", null]);
  });
});
