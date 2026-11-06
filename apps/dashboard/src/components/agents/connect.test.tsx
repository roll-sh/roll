import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { api, resetApi } from "@/test/api";
import { info } from "@/test/fixtures";

vi.mock("@/lib/code-api", async (orig) => ({ ...(await orig()), ...(await import("@/test/api")).apiModule }));
const agents = vi.hoisted(() => ({ approveLogin: vi.fn(), denyLogin: vi.fn(), addMachine: vi.fn(), pendingLogin: vi.fn() }));
vi.mock("@/lib/agents-api", async (orig) => ({ ...(await orig()), agentsApi: agents }));

import { ConnectAgents } from "./connect";

const pending = { user_code: "WXYZ-2345", name: "ada-mbp", hostname: "ada-mbp.local", os: "macos", created_at: 0, expires_at: 900_000 };

describe("ConnectAgents", () => {
  beforeEach(() => {
    resetApi();
    Object.values(agents).forEach((f) => f.mockReset());
    api.info.mockResolvedValue(info({ required: true, caller: { kind: "owner", owner: "acme" } }));
    agents.pendingLogin.mockResolvedValue(pending);
  });

  it("approves a device login for the signed-in owner, saying which machine asks", async () => {
    agents.approveLogin.mockResolvedValue(undefined);
    render(<ConnectAgents code="WXYZ-2345" />);
    expect(screen.getByText("WXYZ-2345")).toBeInTheDocument();
    expect(await screen.findByText("Approve ada-mbp (macOS)?")).toBeInTheDocument();
    expect(agents.pendingLogin).toHaveBeenCalledWith("acme", "WXYZ-2345");
    await waitFor(() => expect(screen.getByRole("combobox", { name: "Owner" })).toHaveTextContent("acme"));
    await userEvent.type(screen.getByRole("textbox", { name: "Machine name" }), "ada-mbp");
    await userEvent.click(screen.getByRole("button", { name: "Connect machine" }));
    expect(agents.approveLogin).toHaveBeenCalledWith("acme", "WXYZ-2345", "ada-mbp");
    expect(await screen.findByText("Connected")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Open your agents" })).toHaveAttribute("href", "#agents");
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
    // The masked command signs in with it: no hand-written agent.json.
    expect(screen.getByText(/roll agent login.* --machine-token rlm_secr…/)).toBeInTheDocument();
    expect(screen.queryByText(/agent\.json/)).toBeNull();
    // The install line comes from this web app.
    expect(screen.getByText(`curl -fsSL ${window.location.origin}/install.sh | sh`)).toBeInTheDocument();
  });
});
