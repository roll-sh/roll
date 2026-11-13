import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { AuditEvent, Grant } from "@/lib/access-api";
import { TokenPage } from "./token-page";

const grant: Grant = {
  id: "tok_1",
  name: "cursor",
  kind: "token",
  rules: [{ product: "postgres", resources: [{ id: "prj_billing" }], actions: { read: "allow" } }],
  temporary: [{ rule: { product: "postgres", resources: [{ id: "prj_billing" }], actions: { write: "allow" } }, until: Date.now() + 3600_000 }],
  expires_at: null,
  ip_allow: [],
  created_by: "operator",
  created_at: 0,
  last_used_at: null,
  last_used_ip: null,
  revoked_at: null,
};
const event = { id: "ev_1", at: Date.now(), grant_id: "tok_1", decision: "deny", method: "DELETE", route: "/v1/projects/{id}", tool: null, resource: "prj_logs", status: 403 } as AuditEvent;
const revoke = vi.fn(async (_id: string) => ({ ...grant, revoked_at: Date.now() }));

vi.mock("@/lib/access-api", async (orig) => ({
  ...(await orig<typeof import("@/lib/access-api")>()),
  accessApi: { token: async () => grant, audit: async () => [event], approvals: async () => [], revoke: (id: string) => revoke(id) },
}));
vi.mock("@/lib/workspace-resources", () => ({
  loadInventory: async () => ({ views: new Map(), kinds: new Set() }),
  resourcesByProduct: () => ({ postgres: [{ id: "prj_billing", name: "billing" }] }),
}));
vi.mock("@/lib/workspace-store", () => ({ useWorkspace: () => ({ projects: [] }) }));
vi.mock("../link", () => ({ Link: ({ children, href }: { children: React.ReactNode; href: string }) => <a href={href}>{children}</a> }));
vi.mock("./shell", async (orig) => ({ ...(await orig<typeof import("./shell")>()), AccessShell: ({ children }: { children: React.ReactNode }) => <>{children}</> }));

/** A token's page: what it may do and for a while by name, what it's asked lately, and revoking it there. */
describe("the token page", () => {
  it("shows the token's rules and requests, and revokes it", async () => {
    render(<TokenPage id="tok_1" />);
    expect(await screen.findByText(/Postgres · read · billing/)).toBeTruthy();
    expect(await screen.findByText(/Postgres · write · billing/)).toBeTruthy();
    expect(await screen.findByText(/DELETE \/v1\/projects\/\{id\} · prj_logs/)).toBeTruthy();
    expect(screen.getByRole("link", { name: "All of its requests" }).getAttribute("href")).toBe("/dashboard/access/audit?grant=tok_1");
    await userEvent.click(screen.getByRole("button", { name: "Revoke" }));
    await userEvent.click(screen.getByRole("button", { name: "Revoke now" }));
    await waitFor(() => expect(revoke).toHaveBeenCalledWith("tok_1"));
  });
});
