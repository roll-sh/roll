import { describe, expect, it } from "vitest";
import { notifyBody, notifyProblem } from "./secrets-notify";

describe("Notify me", () => {
  it("sends the chosen kinds, environment and channel, and every kind when no chip is on", () => {
    expect(notifyBody({ families: ["secrets", "rotations"], environment: "production", target: { kind: "slack", install: "chi_fake1", channel: " C0FAKE01 " } })).toEqual({
      kinds: ["secret.changed", "secret.deleted", "rotation.*"],
      environment: "production",
      channel: { slack: { install: "chi_fake1", channel: "C0FAKE01" } },
    });
    expect(notifyBody({ families: [], target: { kind: "email" } })).toEqual({ channel: { email: "own" } });
    expect(notifyProblem({ families: [], target: { kind: "discord", install: "chi_fake2", channel: "#general" } })).toMatch(/id/);
  });
});
