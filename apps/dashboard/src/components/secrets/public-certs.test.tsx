import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { NewPublicCert, PublicCert, PublicCertChange } from "@/lib/secrets-api";
import { PublicCerts } from "./public-certs";

const TOKEN = "cf-Zx91-secret-token";
const listed: PublicCert[] = [];
const api = {
  publicCerts: vi.fn(async () => listed),
  awsRoles: vi.fn(async () => []),
  addPublicCert: vi.fn(async (c: NewPublicCert) => {
    const made: PublicCert = { id: "pcr_1", name: c.name, directory_url: c.directory_url, domains: c.domains, dns_provider: c.dns_provider, status: "pending", renewals_asked: 0, created_at: 0, created_by: "operator" };
    listed.push(made);
    return made;
  }),
  updatePublicCert: vi.fn(async (id: string, _ch: PublicCertChange) => listed.find((c) => c.id === id)!),
};
vi.mock("./secrets-page", () => ({ useSecrets: () => ({ api, project: { id: "prj_1", name: "shop" } }) }));

describe("adding a public certificate", () => {
  it("sends the DNS credential once and never shows it again", async () => {
    const { container } = render(<PublicCerts />);
    fireEvent.click(await screen.findByRole("button", { name: /Add a public certificate/ }));
    fireEvent.change(screen.getByPlaceholderText("web"), { target: { value: "web" } });
    fireEvent.change(screen.getByPlaceholderText("example.com, *.example.com"), { target: { value: "example.com, *.example.com" } });
    const token = container.querySelector<HTMLInputElement>('input[type="password"]')!;
    fireEvent.change(token, { target: { value: TOKEN } });
    fireEvent.click(screen.getByRole("button", { name: "Add certificate" }));
    await waitFor(() => expect(api.addPublicCert).toHaveBeenCalledWith(expect.objectContaining({ dns_provider: "cloudflare", dns_credential: TOKEN, domains: ["example.com", "*.example.com"] })));
    expect(await screen.findByText("ordering")).toBeTruthy();
    const shown = () => [container.textContent ?? "", ...[...container.querySelectorAll("input")].map((i) => i.value)].join("\n");
    expect(shown()).not.toContain(TOKEN);
    // Opening the form again starts empty: nothing reads the credential back.
    fireEvent.click(screen.getByRole("button", { name: /Add a public certificate/ }));
    expect(container.querySelector<HTMLInputElement>('input[type="password"]')!.value).toBe("");
    expect(shown()).not.toContain(TOKEN);
  });
});

describe("replacing a certificate's DNS credential", () => {
  it("sends the new credential once and never shows it again", async () => {
    const NEW = "cf-Qq72-replaced-token";
    listed.splice(0, listed.length, { id: "pcr_2", name: "api", directory_url: "https://acme-v02.api.letsencrypt.org/directory", domains: ["api.example.com"], dns_provider: "cloudflare", status: "failing", last_error: "Cloudflare: 403", renewals_asked: 0, created_at: 0, created_by: "operator" });
    const { container } = render(<PublicCerts />);
    const open = await screen.findByRole("button", { name: "Replace api's credential" });
    fireEvent.click(open);
    fireEvent.change(container.querySelector<HTMLInputElement>('input[type="password"]')!, { target: { value: NEW } });
    fireEvent.click(screen.getByRole("button", { name: "Replace credential" }));
    await waitFor(() => expect(api.updatePublicCert).toHaveBeenCalledWith("pcr_2", { dns_credential: NEW }));
    await waitFor(() => expect(container.querySelector('input[type="password"]')).toBeNull());
    const shown = () => [container.textContent ?? "", ...[...container.querySelectorAll("input")].map((i) => i.value)].join("\n");
    expect(shown()).not.toContain(NEW);
    // Opened again, the form starts empty: nothing reads the credential back.
    fireEvent.click(screen.getByRole("button", { name: "Replace api's credential" }));
    expect(container.querySelector<HTMLInputElement>('input[type="password"]')!.value).toBe("");
    expect(shown()).not.toContain(NEW);
  });
});
