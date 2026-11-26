import { describe, expect, it } from "vitest";
import { matchesTerms, splitFilter, wordsMatch } from "./log-where";

describe("log filters by a line's fields", () => {
  it("splits field terms from words and matches JSON and logfmt as the API does", () => {
    const f = splitFilter('timeout level:error -user_id:7 msg:"cache miss" https://example.com/x');
    expect(f.where).toBe('level:error -user_id:7 msg:"cache miss"');
    expect(f.words).toBe("timeout https://example.com/x");
    const json = '{"level":"ERROR","user_id":42,"msg":"cache miss","req":{"method":"POST"}}';
    expect(matchesTerms(f.terms, json)).toBe(true);
    expect(matchesTerms(splitFilter("user_id:4").terms, json)).toBe(false);
    expect(matchesTerms(splitFilter("req.method:post user_id:*").terms, json)).toBe(true);
    expect(matchesTerms(splitFilter("level:warn user_id:42").terms, "level=warn user_id=42 done")).toBe(true);
    // An address or a time is a word, not a field.
    expect(splitFilter("localhost 127.0.0.1:5432 12:30").where).toBe("");
  });
});

describe("wordsMatch", () => {
  it("reads words as the server's search does", () => {
    expect(wordsMatch("checkout error", "ERROR in Checkout")).toBe(true);
    expect(wordsMatch('"order 7"', "order 17 of 7")).toBe(false);
    expect(wordsMatch("error -health", "error in /health")).toBe(false);
    expect(wordsMatch("timeout OR refused", "connection refused")).toBe(true);
    expect(wordsMatch("--verbose", "ran with --verbose")).toBe(true);
    expect(wordsMatch("", "anything")).toBe(true);
  });
});
