import { describe, expect, it } from "vitest";
import { keepable, looksSecret } from "./secret-names";

describe("names that read as secrets", () => {
  it("seals keys, tokens and passwords by default, not ordinary settings", () => {
    expect(["STRIPE_SECRET_KEY", "GITHUB_TOKEN", "DB_PASSWORD", "API_KEY", "SENTRY_DSN", "aws_access_key_id"].map(looksSecret)).toEqual([true, true, true, true, true, true]);
    expect(["NODE_ENV", "PORT", "LOG_LEVEL", "KEYBOARD_LAYOUT", "PUBLIC_URL"].map(looksSecret)).toEqual([false, false, false, false, false]);
  });

  it("offers a preview rule's credential a key in roll secrets, not a reference or an ordinary value", () => {
    expect(keepable("STRIPE_SECRET_KEY", "sk_test_51Hx9")).toBe(true);
    expect([keepable("STRIPE_SECRET_KEY", "roll://staging/STRIPE_SECRET_KEY"), keepable("STRIPE_SECRET_KEY", " "), keepable("LOG_LEVEL", "debug")]).toEqual([false, false, false]);
  });
});
