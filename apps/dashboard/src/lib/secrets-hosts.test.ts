import { describe, expect, it } from "vitest";
import { DEFAULT_HEADER, hostsBody, hostsForm } from "./secrets-hosts";

describe("a key's host binding", () => {
  it("always names a placement, the bearer header unless told otherwise", () => {
    expect(hostsBody({ ...hostsForm(), hosts: "api.stripe.com, *.stripe.com" })).toEqual({ hosts: ["api.stripe.com", "*.stripe.com"], header: DEFAULT_HEADER });
    // One bound before a placement was required opens with the default, so saving it gives it one.
    expect(hostsForm({ hosts: ["api.stripe.com"], untrusted: false }).placement).toEqual({ kind: "header", header: DEFAULT_HEADER });
    const query = hostsForm({ hosts: ["api.example.com"], query: "api_key", untrusted: true });
    expect(hostsBody(query)).toEqual({ hosts: ["api.example.com"], query: "api_key", untrusted: true });
    expect(hostsBody({ ...hostsForm(), hosts: "api.stripe.com", placement: { kind: "header", header: "X-Key: {} {}" } })).toMatch(/once/);
  });
});
