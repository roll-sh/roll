import { describe, expect, it } from "vitest";
import type { Secret } from "./secrets-api";
import { parseDotenv, previewImport } from "./secrets-dotenv";

const secret = (key: string, source: Secret["source"] = { kind: "own" }): Secret => ({
  id: `sec_${key}`,
  key,
  path: "/",
  type: "shared",
  version: 1,
  comment: "",
  tags: [],
  link: null,
  fp: "00",
  references: [],
  updated_at: 0,
  updated_by: "operator",
  source,
  overridden: false,
});

describe("the .env bulk paste", () => {
  it("reads a .env the way the server does", () => {
    const text = [
      "﻿# the database",
      "export DATABASE_URL = postgres://a:b@h/db  # trailing comment",
      "",
      "SINGLE='literal \\n ${X}'",
      'PEM="-----BEGIN-----',
      'abc\\"def',
      '-----END-----"',
      "EMPTY=",
      "DATABASE_URL=again",
    ].join("\r\n");
    const { entries, error } = parseDotenv(text);
    expect(error).toBeNull();
    expect(entries.map((e) => [e.key, e.value, e.comment])).toEqual([
      ["DATABASE_URL", "again", ""],
      ["SINGLE", "literal \\n ${X}", ""],
      ["PEM", '-----BEGIN-----\nabc"def\n-----END-----', ""],
      ["EMPTY", "", ""],
    ]);
    expect(parseDotenv("# db\nA=1").entries[0].comment).toBe("db");
    expect(parseDotenv("OK=1\n1BAD=2").error).toMatch(/^line 2: a key/);
    expect(parseDotenv('A="open').error).toBe("line 1: the double quote isn't closed");
    expect(parseDotenv("A=1\nnot a pair").error).toBe("line 2: expected KEY=value");
  });

  it("previews new, replaced and skipped keys against the folder", () => {
    const current = [secret("A"), secret("SHARED", { kind: "import", environment: "staging", path: "/common" })];
    const { entries } = parseDotenv("A=1\nB=2\nSHARED=3");
    expect(previewImport(entries, current, false).map((r) => [r.key, r.action, r.over])).toEqual([
      ["A", "skip", undefined],
      ["B", "new", undefined],
      ["SHARED", "new", "imported from staging/common"],
    ]);
    expect(previewImport(entries, current, true)[0].action).toBe("replace");
  });
});
