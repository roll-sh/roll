import { describe, expect, it } from "vitest";
import { looksSecret } from "./secret-names";

describe("names that read as secrets", () => {
  it("seals keys, tokens and passwords by default, not ordinary settings", () => {
    expect(["STRIPE_SECRET_KEY", "GITHUB_TOKEN", "DB_PASSWORD", "API_KEY", "SENTRY_DSN", "aws_access_key_id"].map(looksSecret)).toEqual([true, true, true, true, true, true]);
    expect(["NODE_ENV", "PORT", "LOG_LEVEL", "KEYBOARD_LAYOUT", "PUBLIC_URL"].map(looksSecret)).toEqual([false, false, false, false, false]);
  });
});
