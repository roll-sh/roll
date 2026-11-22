import { describe, expect, it } from "vitest";
import { branchesHoldingKey, endpointBody } from "./inference";

describe("endpointBody", () => {
  it("puts each service's credentials and settings where its API reads them", () => {
    expect(endpointBody("amazonbedrock", "text_embedding", "amazon.titan-embed-text-v2:0", { access_key: "AK", secret_key: "SK" })).toEqual({
      service: "amazonbedrock",
      service_settings: { model: "amazon.titan-embed-text-v2:0", provider: "amazontitan", region: "us-east-1", access_key: "AK", secret_key: "SK" },
    });
    // Location applies to embeddings, not rerank; a task setting goes in task_settings.
    expect(endpointBody("googlevertexai", "rerank", "", { project_id: "p", service_account_json: "{}" })).toEqual({
      service: "googlevertexai",
      service_settings: { project_id: "p", service_account_json: "{}" },
    });
    expect(endpointBody("anthropic", "completion", "claude", { api_key: "k" }).task_settings).toEqual({ max_tokens: 1024 });
  });
});

describe("branchesHoldingKey", () => {
  it("names the branches copied since the key was bound, by their copy point", () => {
    const branches = [
      { id: "a", name: "before", parent_at: null, created_at: 99 },
      { id: "b", name: "after", parent_at: null, created_at: 101 },
      { id: "c", name: "made-later-copied-earlier", parent_at: 50, created_at: 200 },
    ];
    expect(branchesHoldingKey(branches, 100_500)).toEqual(["after"]);
  });
});
