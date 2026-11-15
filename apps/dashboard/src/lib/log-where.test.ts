import { describe, expect, it } from "vitest";
import { matchesTerms, splitFilter } from "./log-where";

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
