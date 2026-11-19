/**
 * roll search's security audit log (`GET /v1/search/clusters/{id}/audit`,
 * crates/search-server/src/audit.rs): entries as the cluster's product writes them,
 * Elasticsearch's (`event.action`, `user.name`, `origin.address`, …) or OpenSearch Security's
 * (`audit_category`, `audit_request_effective_user`, …), summarized here for one table.
 */

import type { Flavor } from "./search-api";

export interface AuditEntry {
  at_ms: number;
  /** Elasticsearch's include name (`access_denied`) or OpenSearch's category (`FAILED_LOGIN`). */
  event: string;
  entry: Record<string, unknown>;
}

export interface AuditPage {
  flavor: Flavor;
  retention_days: number;
  entries: AuditEntry[];
  /** Older entries matched past the page. */
  more: boolean;
}

/** What the filter offers: what roll records, by default first. */
export const AUDIT_EVENTS: Record<Flavor, string[]> = {
  elasticsearch: ["authentication_failed", "anonymous_access_denied", "access_denied", "run_as_denied", "security_config_change", "access_granted", "authentication_success"],
  opensearch: ["FAILED_LOGIN", "MISSING_PRIVILEGES", "COMPLIANCE_INTERNAL_CONFIG_WRITE", "COMPLIANCE_INTERNAL_CONFIG_READ", "INDEX_EVENT", "GRANTED_PRIVILEGES", "AUTHENTICATED"],
};

/** One table row. */
export interface AuditRow {
  at_ms: number;
  event: string;
  /** Refusals and failures. */
  failure: boolean;
  user: string | null;
  address: string | null;
  /** The action, the change (`put_user`) or the path. */
  action: string | null;
  /** Indices, or what a change was about (`user bob`). */
  target: string | null;
}

const str = (v: unknown) => (typeof v === "string" && v ? v : null);
const list = (v: unknown) => (Array.isArray(v) && v.length ? v.map(String).join(", ") : null);

const FAILURES = new Set(["authentication_failed", "anonymous_access_denied", "access_denied", "run_as_denied", "FAILED_LOGIN", "MISSING_PRIVILEGES"]);

/** `{put: {user: {name: "bob"}}}` → `user bob`. */
function changed(e: Record<string, unknown>): string | null {
  for (const verb of ["put", "delete", "change", "create", "invalidate"]) {
    const what = e[verb];
    if (!what || typeof what !== "object") continue;
    for (const [kind, v] of Object.entries(what as Record<string, unknown>)) {
      const o = (v ?? {}) as Record<string, unknown>;
      const user = (o.user ?? {}) as Record<string, unknown>;
      const name = str(o.name) ?? str(user.name) ?? list(o.ids) ?? str(o.id);
      const noun = kind.replace(/_/g, " ").replace(/^apikeys?$/, "API key");
      if (Array.isArray(v)) return `${noun} ${v.map((p) => str((p as Record<string, unknown>).name)).filter(Boolean).join(", ")}`.trim();
      return name ? `${noun} ${name}` : noun;
    }
  }
  return null;
}

export function auditRow(a: AuditEntry): AuditRow {
  const e = a.entry;
  if (e.audit_category !== undefined) {
    const user = str(e.audit_request_effective_user);
    return {
      at_ms: a.at_ms,
      event: a.event,
      failure: FAILURES.has(a.event),
      user: user === "<NONE>" ? null : user,
      address: str(e.audit_request_remote_address),
      action: str(e.audit_compliance_operation) ? `${e.audit_compliance_operation} ${e.audit_trace_doc_id}` : (str(e.audit_request_privilege) ?? str(e.audit_rest_request_path)),
      target: list(e.audit_trace_indices) ?? str(e.audit_rest_request_path),
    };
  }
  const config = a.event === "security_config_change";
  const runAs = str(e["user.run_as.name"]);
  const user = str(e["user.name"]);
  return {
    at_ms: a.at_ms,
    event: a.event,
    failure: FAILURES.has(a.event),
    user: user && runAs ? `${user} → ${runAs}` : user,
    address: str(e["origin.address"]),
    action: config ? str(e["event.action"]) : (str(e.action) ?? (str(e["url.path"]) && `${e["request.method"] ?? ""} ${e["url.path"]}`.trim())),
    target: config ? changed(e) : list(e.indices),
  };
}

/** `access_denied` → `access denied`; OpenSearch's categories read as they are. */
export function eventLabel(event: string): string {
  return event === event.toUpperCase() ? event : event.replace(/_/g, " ");
}
