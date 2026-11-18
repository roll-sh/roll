import { describe, expect, it } from "vitest";
import { auditRow, eventLabel } from "./search-audit";

describe("auditRow", () => {
  it("reads Elasticsearch's entries", () => {
    const denied = auditRow({
      at_ms: 1,
      event: "access_denied",
      entry: { "event.action": "access_denied", "user.name": "bob", "origin.address": "10.0.0.7:5544", action: "indices:data/read/search", indices: ["secret"] },
    });
    expect(denied).toMatchObject({ failure: true, user: "bob", address: "10.0.0.7:5544", action: "indices:data/read/search", target: "secret" });
    const failed = auditRow({ at_ms: 2, event: "authentication_failed", entry: { "user.name": "bob", "url.path": "/_search", "request.method": "GET" } });
    expect(failed).toMatchObject({ failure: true, user: "bob", action: "GET /_search", target: null });
    const put = auditRow({ at_ms: 3, event: "security_config_change", entry: { "event.action": "put_user", put: { user: { name: "bob", has_password: true } } } });
    expect(put).toMatchObject({ failure: false, action: "put_user", target: "user bob" });
    const key = auditRow({ at_ms: 4, event: "security_config_change", entry: { "event.action": "invalidate_apikeys", invalidate: { apikeys: { ids: ["k1"] } } } });
    expect(key.target).toBe("API key k1");
    const runAs = auditRow({ at_ms: 5, event: "run_as_denied", entry: { "user.name": "bob", "user.run_as.name": "elastic" } });
    expect(runAs.user).toBe("bob → elastic");
  });

  it("reads OpenSearch's entries", () => {
    const anon = auditRow({ at_ms: 1, event: "FAILED_LOGIN", entry: { audit_category: "FAILED_LOGIN", audit_request_effective_user: "<NONE>", audit_rest_request_path: "/" } });
    expect(anon).toMatchObject({ failure: true, user: null, action: "/" });
    const write = auditRow({
      at_ms: 2,
      event: "COMPLIANCE_INTERNAL_CONFIG_WRITE",
      entry: { audit_category: "COMPLIANCE_INTERNAL_CONFIG_WRITE", audit_request_effective_user: "admin", audit_compliance_operation: "UPDATE", audit_trace_doc_id: "roles", audit_trace_indices: [".opendistro_security"] },
    });
    expect(write).toMatchObject({ failure: false, user: "admin", action: "UPDATE roles" });
    expect(eventLabel("access_denied")).toBe("access denied");
    expect(eventLabel("FAILED_LOGIN")).toBe("FAILED_LOGIN");
  });
});
