import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Approval } from "@/lib/access-api";
import { WaitingApprovals } from "./waiting";

const approve = vi.fn(async (_id: string) => ({}));
const deny = vi.fn(async (_id: string) => ({}));
vi.mock("@/lib/access-api", async (orig) => ({ ...(await orig<typeof import("@/lib/access-api")>()), accessApi: { approve: (id: string) => approve(id), deny: (id: string) => deny(id) } }));
vi.mock("../link", () => ({ Link: ({ children, href }: { children: React.ReactNode; href: string }) => <a href={href}>{children}</a> }));

const waiting = (id: string, action: string): Approval =>
  ({ id, grant_id: "tok_1", grant_name: "cursor", product: "postgres", action, resource: "prj_billing", method: "DELETE", route: "/v1/projects/{id}", state: "pending" }) as Approval;
vi.mock("./use-access", () => ({ usePending: () => [waiting("apr_1", "delete"), waiting("apr_2", "write")] }));

/** The top bar's waiting badge opens the requests, to approve or deny them right there. */
describe("waiting approvals", () => {
  it("approves a request from the top bar, and it leaves the list", async () => {
    render(<WaitingApprovals />);
    await userEvent.click(screen.getByRole("button", { name: /2\s*waiting/ }));
    await userEvent.click(screen.getAllByRole("button", { name: "Approve" })[0]);
    expect(approve).toHaveBeenCalledWith("apr_1");
    await waitFor(() => expect(screen.getByRole("button", { name: /1\s*waiting/ })).toBeTruthy());
    await userEvent.click(screen.getByRole("button", { name: "Deny" }));
    expect(deny).toHaveBeenCalledWith("apr_2");
  });
});
