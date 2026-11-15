import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { LogDrain } from "@/lib/apps-api";
import { DrainsPanel } from "./drains";
import type { Runner } from "./shared";

const api = vi.hoisted(() => ({ drains: vi.fn(), services: vi.fn(), updateDrain: vi.fn(), addDrain: vi.fn() }));
vi.mock("@/lib/apps-api", async (orig) => ({ ...(await orig<typeof import("@/lib/apps-api")>()), appsApi: api }));

const runner: Runner = { busy: null, error: null, failed: null, setError: () => {}, run: async (_k, fn) => fn() } as Runner;
const drain = { id: "drn_1", host: "http-intake.logs.example.com", sources: ["runtime"], environments: ["production"], services: [], previews: false, headers: ["DD-API-KEY"], created_at: 1 } as unknown as LogDrain;

describe("DrainsPanel", () => {
  it("edits a drain's sources and filters, keeping its URL and its sealed headers", async () => {
    api.drains.mockResolvedValue([drain]);
    api.services.mockResolvedValue([]);
    api.updateDrain.mockResolvedValue(drain);
    render(<DrainsPanel project="prj_1" projectName="shop" runner={runner} />);
    await userEvent.click(await screen.findByRole("button", { name: /edit/i }));
    expect(screen.getByLabelText("Drain URL")).toHaveAttribute("readonly");
    await userEvent.click(screen.getByLabelText("Requests"));
    await userEvent.clear(screen.getByLabelText(/only these environments/i));
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(api.updateDrain).toHaveBeenCalled());
    // A cleared filter goes back to every environment; no headers sent, so the key stays.
    expect(api.updateDrain).toHaveBeenCalledWith("prj_1", "drn_1", { sources: ["runtime", "http"], environments: [], services: [], previews: false });
    expect(api.addDrain).not.toHaveBeenCalled();
  });
});
