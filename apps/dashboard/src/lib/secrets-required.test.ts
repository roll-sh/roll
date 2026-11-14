import { describe, expect, it } from "vitest";
import { misfit } from "./secrets-required";

describe("typed keys", () => {
  it("say what the server says, and leave references to it", () => {
    const port = { key: "PORT", path: "/", comment: "", type: "port" as const };
    expect(misfit(port, "eighty-eighty")).toBe("PORT must be a port (1–65535)");
    expect(misfit(port, "8080")).toBeNull();
    expect(misfit(port, "${staging.PORT}")).toBeNull();
    expect(misfit({ key: "URL", path: "/", comment: "", type: "url" }, "shop.example")).toBe("URL must be a URL");
    const region = { key: "REGION", path: "/", comment: "", pattern: "[a-z]{2}-[a-z]+-[0-9]" };
    expect(misfit(region, "Europe")).toBe("REGION must match [a-z]{2}-[a-z]+-[0-9]");
    expect(misfit(region, "eu-west-1")).toBeNull();
  });
});
