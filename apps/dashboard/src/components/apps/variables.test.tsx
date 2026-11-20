import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Environment } from "@/lib/apps-api";
import { VariablesTab } from "./variables";
import type { Runner } from "./shared";

const api = vi.hoisted(() => ({ variables: vi.fn(), setVariables: vi.fn(), reveal: vi.fn(), services: vi.fn(), links: vi.fn(), link: vi.fn(), unlink: vi.fn(), pending: vi.fn() }));
vi.mock("@/lib/apps-api", async (orig) => ({ ...(await orig<typeof import("@/lib/apps-api")>()), appsApi: api }));
const sec = vi.hoisted(() => ({ set: vi.fn() }));
vi.mock("@/lib/secrets-api", async (orig) => ({ ...(await orig<typeof import("@/lib/secrets-api")>()), secretsApi: () => sec }));

const env = { name: "production" } as unknown as Environment;
const runner: Runner = { busy: null, error: null, failed: null, setError: () => {}, run: async (_k, fn) => fn() } as Runner;
const listed = { vars: [{ name: "MODE", masked: "••••", source: "service" }, { name: "API_KEY", masked: "••••", source: "service", sealed: true }] };

describe("VariablesTab", () => {
  beforeEach(() => {
    api.variables.mockResolvedValue(listed);
    api.services.mockResolvedValue([]);
    api.links.mockResolvedValue([]);
    api.pending.mockResolvedValue({});
    api.setVariables.mockResolvedValue(listed);
  });

  it("seals a new secret-looking variable by default, and says it's for good", async () => {
    render(<VariablesTab service="svc_1" project="prj_1" env={env} runner={runner} onRedeploy={() => {}} />);
    expect(await screen.findByText("MODE")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /add/i }));
    await userEvent.type(screen.getByPlaceholderText("NAME"), "stripe_secret");
    await userEvent.type(screen.getByPlaceholderText(/value, or/), "sk_live_1");
    expect(screen.getByRole("button", { name: "Seal" })).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByText(/Sealing STRIPE_SECRET is for good/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.setVariables).toHaveBeenCalledWith("svc_1", "production", { set: { STRIPE_SECRET: "sk_live_1" }, unset: [], seal: ["STRIPE_SECRET"] }));
  });

  it("moves a secret-looking variable into roll secrets, leaving a reference in its place", async () => {
    api.variables.mockResolvedValue({ vars: [...listed.vars, { name: "STRIPE_KEY", masked: "••••", source: "service" }] });
    api.reveal.mockResolvedValue({ MODE: "prod", STRIPE_KEY: "sk_1" });
    sec.set.mockResolvedValue({});
    render(<VariablesTab service="svc_1" project="prj_1" env={env} runner={runner} onRedeploy={() => {}} />);
    await screen.findByText("STRIPE_KEY");
    expect(screen.queryByRole("button", { name: "Move MODE to roll secrets" })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Move STRIPE_KEY to roll secrets" }));
    expect(sec.set).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: /sure\? move stripe_key/i }));
    await waitFor(() => expect(api.setVariables).toHaveBeenCalledWith("svc_1", "production", { set: { STRIPE_KEY: "roll://production/services/svc_1/STRIPE_KEY" } }));
    expect(sec.set).toHaveBeenCalledWith("production", "STRIPE_KEY", { value: "sk_1", message: "moved from roll apps svc_1" }, "/services/svc_1");
  });

  it("doesn't move a variable into a protected environment's secrets: it says how", async () => {
    const { SecretsError } = await import("@/lib/secrets-api");
    api.variables.mockResolvedValue({ vars: [{ name: "STRIPE_KEY", masked: "••••", source: "service" }] });
    api.reveal.mockResolvedValue({ STRIPE_KEY: "sk_1" });
    sec.set.mockRejectedValue(new SecretsError("needs approval", 409, "approval_required"));
    const failed: unknown[] = [];
    const r = { ...runner, run: async (_k: string, fn: () => Promise<unknown>) => fn().catch((e) => (failed.push(e), undefined)) } as Runner;
    render(<VariablesTab service="svc_1" project="prj_1" env={env} runner={r} onRedeploy={() => {}} />);
    await userEvent.click(await screen.findByRole("button", { name: "Move STRIPE_KEY to roll secrets" }));
    await userEvent.click(screen.getByRole("button", { name: /sure\? move stripe_key/i }));
    await waitFor(() => expect(String(failed[0])).toContain("approved request"));
    expect(api.setVariables).not.toHaveBeenCalled();
  });

  it("reveals the values it may, never a sealed one's", async () => {
    api.reveal.mockResolvedValue({ MODE: "prod" });
    render(<VariablesTab service="svc_1" project="prj_1" env={env} runner={runner} onRedeploy={() => {}} />);
    await screen.findByText("MODE");
    await userEvent.click(screen.getByRole("button", { name: "Reveal" }));
    expect(await screen.findByText("prod")).toBeInTheDocument();
    expect(screen.queryByText("sk_live_1")).not.toBeInTheDocument();
  });
});
