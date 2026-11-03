import { screen } from "@testing-library/react";
import { render } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "@/lib/code-api";
import { api, resetApi } from "@/test/api";
import { info } from "@/test/fixtures";

vi.mock("@/lib/code-api", async (orig) => ({ ...(await orig()), ...(await import("@/test/api")).apiModule }));
const agents = vi.hoisted(() => ({ approveLogin: vi.fn(), denyLogin: vi.fn(), addMachine: vi.fn() }));
vi.mock("@/lib/agents-api", async (orig) => ({ ...(await orig()), agentsApi: agents }));

import { ConnectAgents } from "./connect";

describe("ConnectAgents", () => {
  beforeEach(() => {
    resetApi();
    Object.values(agents).forEach((f) => f.mockReset());
    api.info.mockResolvedValue(info({ required: true, caller: { kind: "owner", owner: "acme" } }));
  });

  it("approves a device login for the signed-in owner", async () => {
    agents.approveLogin.mockResolvedValue(undefined);
    render(<ConnectAgents code="WXYZ-2345" />);
    expect(screen.getByText("WXYZ-2345")).toBeInTheDocument();
    await screen.findByRole("option", { name: "acme" });
    await userEvent.type(screen.getByRole("textbox", { name: "Machine name" }), "ada-mbp");
    await userEvent.click(screen.getByRole("button", { name: "Connect machine" }));
    expect(agents.approveLogin).toHaveBeenCalledWith("acme", "WXYZ-2345", "ada-mbp");
    expect(await screen.findByText("Connected")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Open the canvas" })).toHaveAttribute("href", "/dashboard/agents?owner=acme");
  });

  it("says so when the code isn't waiting any more", async () => {
    agents.denyLogin.mockRejectedValue(new ApiError("no pending login", 404, null));
    render(<ConnectAgents code="BCDF-GHJK" />);
    await screen.findByRole("option", { name: "acme" });
    await userEvent.click(screen.getByRole("button", { name: "Deny" }));
    expect(agents.denyLogin).toHaveBeenCalledWith("acme", "BCDF-GHJK");
    expect(await screen.findByText("This code isn't waiting any more")).toBeInTheDocument();
  });

  it("mints a machine token and shows it once", async () => {
    agents.addMachine.mockResolvedValue({
      machine: { id: "mch_1", name: "build-box", online: false, info: null, sandbox: null, created_at: 0, last_seen: null },
      token: "rlm_secret",
    });
    render(<ConnectAgents />);
    expect(screen.getByText("roll agent login")).toBeInTheDocument();
    await userEvent.type(screen.getByRole("textbox", { name: "Machine name" }), "build-box");
    await vi.waitFor(() => expect(screen.getByRole("button", { name: "Add machine" })).toBeEnabled());
    await userEvent.click(screen.getByRole("button", { name: "Add machine" }));
    expect(agents.addMachine).toHaveBeenCalledWith("acme", "build-box");
    expect(await screen.findByText("rlm_secret")).toBeInTheDocument();
  });
});
