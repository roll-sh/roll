import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { deriveBrowserKey, signDerivedKey } from "./search-derive";

// The vector search-es's verifier and roll-cli check too.
const vector = JSON.parse(readFileSync(join(__dirname, "..", "..", "..", "..", "crates", "search-es", "tests", "fixtures", "derived-key-vector.json"), "utf8")) as {
  secret: string;
  payload: string;
  key: string;
};

describe("derived browser keys", () => {
  it("sign as the cluster checks them", async () => {
    expect(await signDerivedKey(vector.secret, vector.payload)).toBe(vector.key);
    expect(await deriveBrowserKey(vector.secret, JSON.parse(vector.payload))).toBe(vector.key);
  });
});
