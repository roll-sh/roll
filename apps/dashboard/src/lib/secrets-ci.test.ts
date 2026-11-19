import { describe, expect, it } from "vitest";
import { ciSnippets, GITHUB, GITLAB } from "./secrets-ci";

const at = { api: "https://api.roll.example", env: "production", install: "curl -fsSL https://roll.sh/install.sh | sh" };

describe("CI snippets", () => {
  it("log a fresh GitHub runner in through the action, with no CLI to install", () => {
    const [action] = ciSnippets({ id: "idn_1", issuer: GITHUB, audience: at.api }, at);
    expect(action.text).toContain("uses: roll-sh/roll/sdk/github-action@main");
    expect(action.text).toContain("identity: idn_1");
    expect(action.text).toContain("id-token: write");
  });

  it("give GitLab's job a token for the identity's audience, where the CLI reads it", () => {
    const [job] = ciSnippets({ id: "idn_2", issuer: GITLAB, audience: "roll-prod" }, at);
    expect(job.text).toMatch(/id_tokens:\n\s+ROLL_OIDC_TOKEN:\n\s+aud: roll-prod\n/);
    expect(job.text).toContain(at.install);
  });
});
