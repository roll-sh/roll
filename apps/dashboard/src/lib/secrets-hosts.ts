import type { Hosts } from "@roll/secrets/types";

/** Where a binding's placeholder goes unless an admin says otherwise (docs/secrets.md, "Using a key without seeing it"). */
export const DEFAULT_HEADER = "Authorization: Bearer {}";

export type Placement = { kind: "header"; header: string } | { kind: "query"; query: string };

export type HostsForm = { hosts: string; placement: Placement; untrusted: boolean };

/** The binding form for a key: its own binding, or a new one in the default header. A binding from before a placement was
 * required opens with the default too, so saving it gives it one. */
export function hostsForm(b?: Hosts): HostsForm {
  const placement: Placement = b?.query !== undefined ? { kind: "query", query: b.query } : { kind: "header", header: b?.header ?? DEFAULT_HEADER };
  return { hosts: (b?.hosts ?? []).join("\n"), placement, untrusted: !!b?.untrusted };
}

/** The body of `PUT …/secrets/{key}/hosts`, or why it can't be sent: it always names a placement. */
export function hostsBody(f: HostsForm): Hosts | string {
  const hosts = f.hosts.split(/[\s,]+/).filter(Boolean);
  if (!hosts.length) return "Name a host (api.stripe.com) or a one-label wildcard (*.stripe.com)";
  if (hosts.length > 16) return "16 hosts at most";
  const untrusted = f.untrusted ? { untrusted: true } : {};
  if (f.placement.kind === "header") {
    const header = f.placement.header.trim();
    if (!/^[^:\s]+:/.test(header) || header.split("{}").length !== 2) return "A header with {} once where the key goes, as Authorization: Bearer {}";
    return { hosts, header, ...untrusted };
  }
  const query = f.placement.query.trim();
  if (!/^[A-Za-z0-9_.~-]{1,64}$/.test(query)) return "A query parameter's name, as api_key";
  return { hosts, query, ...untrusted };
}
