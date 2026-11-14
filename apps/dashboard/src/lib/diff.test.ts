import { describe, expect, it } from "vitest";
import { diffLines, parsePatch, splitRows } from "./diff";

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

describe("diffLines", () => {
  it("keeps common lines and pairs a changed one for the split view", () => {
    const rows = diffLines("A=1\nB=2\nC=3", "A=1\nB=20\nC=3\nD=4");
    expect(rows.map((r) => [r.kind, r.text])).toEqual([["ctx", "A=1"], ["del", "B=2"], ["add", "B=20"], ["ctx", "C=3"], ["add", "D=4"]]);
    expect(splitRows(rows)[1].map((s) => s?.text)).toEqual(["B=2", "B=20"]);
  });
});
