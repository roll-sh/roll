import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Checkup } from "@/lib/access-api";
import { AccessOverview } from "./overview";

const checkup: Checkup[] = [
  {
    grant: { id: "tok_old", name: "old-ci", kind: "token", last_used_at: null, expires_at: null, created_at: 0 },
    findings: [{ kind: "unused", why: "it never made a request in 90 days", fix: "revoke" }],
  },
  {
    grant: { id: "tok_root", name: "root", kind: "token", last_used_at: Date.now(), expires_at: null, created_at: 0 },
    findings: [
      { kind: "everything", why: "it may delete, reveal or administer every product's resources without asking", fix: "tighten" },
      { kind: "never_expires", why: "it never expires", fix: "expire" },
    ],
  },
];
const revoke = vi.fn(async (_id: string) => ({}));

vi.mock("@/lib/access-api", async (orig) => ({
  ...(await orig<typeof import("@/lib/access-api")>()),
  accessApi: {
    toolsetCosts: async () => [],
    tokens: async () => [],
    me: async () => ({ kind: "operator", locked: true }),
    checkup: async () => checkup,
    catalog: async () => ({ products: ["postgres", "redis", "access"], actions: [], presets: [], routes: [], operator_only: [], api_url: "" }),
    revoke: (id: string) => revoke(id),
    policy: async () => ({ max_days: null, require_expiry: false, max_idle_days: null }),
    notify: async () => ({ chat: null }),
  },
}));
vi.mock("@/lib/workspace-store", () => ({ useWorkspace: () => ({ projects: [] }) }));
vi.mock("../link", () => ({ Link: ({ children, href, className }: { children: React.ReactNode; href: string; className?: string }) => <a href={href} className={className}>{children}</a> }));
vi.mock("./shell", async (orig) => ({ ...(await orig<typeof import("./shell")>()), AccessShell: ({ children }: { children: React.ReactNode }) => <>{children}</> }));

/**
 * The access page: what needs attention comes first, most serious first, and an unused token is
 * revoked right there; the connect panel's toolsets narrow the MCP URL a client is given.
 */
describe("the access page", () => {
  it("leads with what needs attention, and gives clients a narrowed URL", async () => {
    render(<AccessOverview />);
    const panel = (await screen.findByText(/Needs attention \(2\)/)).closest("section") ?? document.body;
    const names = within(panel as HTMLElement).getAllByRole("link").map((a) => a.textContent);
    expect(names.indexOf("root")).toBeLessThan(names.indexOf("old-ci"));
    await userEvent.click(within(panel as HTMLElement).getByRole("button", { name: "Revoke" }));
    await waitFor(() => expect(revoke).toHaveBeenCalledWith("tok_old"));

    await userEvent.click(await screen.findByRole("button", { name: "Redis" }));
    expect(screen.getAllByText(/\/v1\/mcp\?toolsets=redis/).length).toBeGreaterThan(0);
  });
});
