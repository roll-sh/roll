import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { Identity, MachineChain, NewBootstrap, NewIdentity } from "@/lib/secrets-api";
import { Identities } from "./identities";

const racks = { id: "idn_racks", name: "racks", kind: "machine", issuer: "roll", audience: "", subject: "machine", claims: {}, environments: ["production"], paths: null, actions: ["read", "reveal"], ttl_minutes: 15, created_at: 1, created_by: "usr_ada", last_used_at: null, last_subject: null, allowed_ips: null, disabled_at: null, disabled_why: null } satisfies Identity;
const waiting: MachineChain = { chain: "tok_chain1", state: "waiting", credential_hours: 48, created_at: Date.now(), created_by: "usr_ada", refreshed_at: null, expires_at: Date.now() + 600_000, revoked_at: null, revoked_why: null };

const api = {
  identities: vi.fn<() => Promise<Identity[]>>().mockResolvedValueOnce([]).mockResolvedValue([racks]),
  gateways: vi.fn(async () => []),
  createIdentity: vi.fn(async (_: NewIdentity) => racks),
  createBootstrap: vi.fn(async (_id: string, _b: NewBootstrap) => ({ identity: racks.id, chain: waiting.chain, secret: "rlbt_once_only", expires_at: waiting.expires_at! })),
  machines: vi.fn(async () => [waiting]),
};
vi.mock("./secrets-page", () => ({ useSecrets: () => ({ api, project: { id: "prj_1", name: "shop" }, standard: [{ slug: "production" }] }) }));

describe("a machine identity", () => {
  it("makes a bootstrap token that's shown once, and lists the machine without it", async () => {
    render(<Identities />);
    fireEvent.click(await screen.findByRole("button", { name: /New identity/ }));
    fireEvent.click(screen.getByRole("button", { name: "Machine (bootstrap token)" }));
    fireEvent.change(screen.getByPlaceholderText("deploy"), { target: { value: "racks" } });
    fireEvent.click(screen.getByRole("button", { name: "Create identity" }));
    await waitFor(() => expect(api.createIdentity).toHaveBeenCalledWith(expect.objectContaining({ name: "racks", kind: "machine", environments: ["production"] })));

    // Made right away from the new identity, with its minutes, credential hours and addresses.
    fireEvent.change(await screen.findByLabelText("Credential hours (1–720)"), { target: { value: "48" } });
    fireEvent.change(screen.getByLabelText("Allowed addresses (CIDRs; empty for any)"), { target: { value: "198.51.100.7/32" } });
    fireEvent.click(screen.getByRole("button", { name: "Make token" }));
    const shown = await screen.findByText("rlbt_once_only");
    expect(api.createBootstrap).toHaveBeenCalledWith("idn_racks", { minutes: 10, credential_hours: 48, allowed_ips: ["198.51.100.7/32"] });

    // Done: gone for good. The machine's list has its chain, never the token.
    fireEvent.click(within(shown.closest("[role=status]") as HTMLElement).getByRole("button", { name: "Done" }));
    expect(await screen.findByText("tok_chain1")).toBeTruthy();
    expect(document.body.textContent).not.toContain("rlbt_once_only");
    expect(api.createBootstrap).toHaveBeenCalledTimes(1);
  });
});
