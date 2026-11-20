import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Environment } from "@/lib/apps-api";
import { VariablesTab } from "./variables";
import type { Runner } from "./shared";

const api = vi.hoisted(() => ({ variables: vi.fn(), setVariables: vi.fn(), reveal: vi.fn(), services: vi.fn(), links: vi.fn(), link: vi.fn(), unlink: vi.fn(), pending: vi.fn() }));
vi.mock("@/lib/apps-api", async (orig) => ({ ...(await orig<typeof import("@/lib/apps-api")>()), appsApi: api }));

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

  it("reveals the values it may, never a sealed one's", async () => {
    api.reveal.mockResolvedValue({ MODE: "prod" });
    render(<VariablesTab service="svc_1" project="prj_1" env={env} runner={runner} onRedeploy={() => {}} />);
    await screen.findByText("MODE");
    await userEvent.click(screen.getByRole("button", { name: "Reveal" }));
    expect(await screen.findByText("prod")).toBeInTheDocument();
    expect(screen.queryByText("sk_live_1")).not.toBeInTheDocument();
  });
});
