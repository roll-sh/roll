import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { StatusPagePanel } from "./status-page";
import type { Runner } from "./shared";

const api = vi.hoisted(() => ({ statusPage: vi.fn(), incidents: vi.fn(), updateIncident: vi.fn(), postIncident: vi.fn(), putStatusPage: vi.fn(), deleteStatusPage: vi.fn(), statusDomain: vi.fn(async () => Promise.reject(new Error("none"))) }));
vi.mock("@/lib/apps-api", async (orig) => ({ ...(await orig<typeof import("@/lib/apps-api")>()), appsApi: api }));

const runner: Runner = { busy: null, error: null, failed: null, setError: () => {}, run: async (_k, fn) => fn() } as Runner;

describe("StatusPagePanel", () => {
  it("lists the page's incidents and updates an open one", async () => {
    api.statusPage.mockResolvedValue({ title: "Shop status", url: "https://status-shop.roll.test", items: [] });
    api.incidents.mockResolvedValue([
      { id: "inc_1", title: "Slow checkouts", state: "investigating", started_at: Date.now() - 60_000, resolved_at: null, automatic: false, updates: [{ at: Date.now(), state: "investigating", text: "Looking into it" }] },
      { id: "inc_0", title: "API down", state: "resolved", started_at: Date.now() - 86_400_000, resolved_at: Date.now(), automatic: true, updates: [] },
    ]);
    api.updateIncident.mockResolvedValue(null);
    render(<StatusPagePanel project="prj_1" projectName="shop" runner={runner} />);
    await screen.findByText("Slow checkouts");
    expect(screen.getByText("API down")).toBeInTheDocument();
    // Only the open one can be updated.
    expect(screen.queryByRole("textbox", { name: "API down's update" })).toBeNull();
    await userEvent.selectOptions(screen.getByRole("combobox", { name: "Slow checkouts's state" }), "resolved");
    await userEvent.type(screen.getByRole("textbox", { name: "Slow checkouts's update" }), "Fixed");
    await userEvent.click(screen.getByRole("button", { name: "Post update" }));
    await waitFor(() => expect(api.updateIncident).toHaveBeenCalledWith("prj_1", "inc_1", { state: "resolved", text: "Fixed" }));
    expect(api.incidents).toHaveBeenCalledTimes(2);
  });
});
