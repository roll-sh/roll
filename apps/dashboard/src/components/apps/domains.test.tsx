import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Domain, Environment } from "@/lib/apps-api";
import { DomainsTab } from "./domains";
import type { Runner } from "./shared";

const api = vi.hoisted(() => ({ addDomain: vi.fn(), verifyDomain: vi.fn(), removeDomain: vi.fn(), redirectDomain: vi.fn(), domainPort: vi.fn(), putEnvironment: vi.fn() }));
vi.mock("@/lib/apps-api", async (orig) => ({ ...(await orig<typeof import("@/lib/apps-api")>()), appsApi: api }));

const runner: Runner = { busy: null, error: null, failed: null, setError: () => {}, run: async (_k, fn) => fn() } as Runner;
const domain = (hostname: string, over: Partial<Domain> = {}): Domain =>
  ({ hostname, kind: "custom", record: "cname", status: "pending", target: "web-x.roll.test", token: "tok", error: null, tls: "none", tls_error: null, created_at: 0, ...over }) as Domain;
const env = (domains: Domain[]) => ({ name: "production", url: "https://web-x.roll.test", domains }) as unknown as Environment;

describe("DomainsTab", () => {
  it("adds a domain only once its name is one, and verifies a pending one", async () => {
    api.addDomain.mockResolvedValue({});
    api.verifyDomain.mockResolvedValue({});
    render(<DomainsTab service="svc_1" env={env([domain("shop.example.com")])} runner={runner} />);
    const add = screen.getByRole("button", { name: /add domain/i });
    const field = screen.getByPlaceholderText(/www\.example\.com/);
    await userEvent.type(field, "not a host");
    expect(add).toBeDisabled();
    await userEvent.clear(field);
    await userEvent.type(field, "WWW.Example.com");
    await userEvent.click(add);
    await waitFor(() => expect(api.addDomain).toHaveBeenCalledWith("svc_1", "production", "www.example.com"));
    await userEvent.click(screen.getByRole("button", { name: /verify/i }));
    await waitFor(() => expect(api.verifyDomain).toHaveBeenCalledWith("svc_1", "production", "shop.example.com"));
  });
});
