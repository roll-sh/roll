import { describe, expect, it } from "vitest";
import { HIGHLIGHT_MAX_BYTES, highlight, languageFor } from "./highlight";

describe("languageFor", () => {
  it("maps names and extensions to shiki languages", () => {
    expect(languageFor("src/main.rs")).toBe("rust");
    expect(languageFor("a/b.mts")).toBe("typescript");
    expect(languageFor("Dockerfile.prod")).toBe("docker");
    expect(languageFor("Makefile")).toBe("make");
    expect(languageFor("Cargo.lock")).toBe("toml");
    expect(languageFor("x.yml")).toBe("yaml");
    expect(languageFor("notes")).toBeNull();
    expect(languageFor("data.unknownext")).toBeNull();
  });
});

describe("highlight", () => {
  it("tokenizes into monochrome classes, merging neighbours", async () => {
    const lines = await highlight('const x = "hi";\nfunction f() {}', "typescript");
    expect(lines).toHaveLength(2);
    expect(lines![0].map((t) => t.t).join("")).toBe('const x = "hi";');
    const classes = new Set(lines!.flat().map((t) => t.c).filter(Boolean));
    expect([...classes].every((c) => c!.startsWith("sh-"))).toBe(true);
    expect(classes.size).toBeGreaterThan(1);
    for (const line of lines!) for (let i = 1; i < line.length; i++) expect(line[i].c).not.toBe(line[i - 1].c);
  });

  it("skips unknown languages and huge files", async () => {
    expect(await highlight("x", null)).toBeNull();
    expect(await highlight("x".repeat(HIGHLIGHT_MAX_BYTES + 1), "typescript")).toBeNull();
    expect(await highlight("x", "not-a-language")).toBeNull();
  });
});
