import { screen } from "@testing-library/react";
import { render } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "@/lib/code-api";
import { api, resetApi } from "@/test/api";
import { info } from "@/test/fixtures";

vi.mock("@/lib/code-api", async (orig) => ({ ...(await orig()), ...(await import("@/test/api")).apiModule }));
const agents = vi.hoisted(() => ({ approveLogin: vi.fn(), denyLogin: vi.fn(), addMachine: vi.fn(), pendingLogin: vi.fn() }));
vi.mock("@/lib/agents-api", async (orig) => ({ ...(await orig()), agentsApi: agents }));

import { apiFlag, ConnectAgents, machineLabel, tokenLogin } from "./connect";

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
    await screen.findByRole("option", { name: "acme" });
    await userEvent.type(screen.getByRole("textbox", { name: "Machine name" }), "ada-mbp");
    await userEvent.click(screen.getByRole("button", { name: "Connect machine" }));
    expect(agents.approveLogin).toHaveBeenCalledWith("acme", "WXYZ-2345", "ada-mbp");
    expect(await screen.findByText("Connected")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Open the canvas" })).toHaveAttribute("href", "/dashboard/agents?owner=acme");
  });

  it("says at once when no login is waiting with the code", async () => {
    agents.pendingLogin.mockRejectedValue(new ApiError("login not found", 404, null));
    render(<ConnectAgents code="BCDF-GHJK" />);
    expect(await screen.findByText("This code isn't waiting any more")).toBeInTheDocument();
    expect(agents.approveLogin).not.toHaveBeenCalled();
  });

  it("still approves when the lookup fails for another reason", async () => {
    agents.pendingLogin.mockRejectedValue(new ApiError("unavailable", 503, null));
    agents.approveLogin.mockResolvedValue(undefined);
    render(<ConnectAgents code="WXYZ-2345" />);
    await screen.findByRole("option", { name: "acme" });
    expect(screen.getByText("A machine wants to connect")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Connect machine" }));
    expect(agents.approveLogin).toHaveBeenCalledWith("acme", "WXYZ-2345", undefined);
  });

  it("says so when the code isn't waiting any more", async () => {
    agents.pendingLogin.mockReturnValue(new Promise(() => {}));
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
    // The masked command signs in with it: no hand-written agent.json.
    expect(screen.getByText(/roll agent login.* --machine-token rlm_secr…/)).toBeInTheDocument();
    expect(screen.queryByText(/agent\.json/)).toBeNull();
    // The install line comes from this web app.
    expect(screen.getByText(`curl -fsSL ${window.location.origin}/install.sh | sh`)).toBeInTheDocument();
  });

  it("builds the CLI lines", () => {
    const m = { machine: { id: "mch_1", name: "b", online: false, info: null, sandbox: null, created_at: 0, last_seen: null }, token: "rlm_0123456789" };
    expect(tokenLogin(m, false, "https://api.roll.sh/")).toBe("roll agent login --api https://api.roll.sh --machine-token rlm_0123456789\nroll agent up");
    expect(tokenLogin(m, true, "http://127.0.0.1:7070")).toBe("roll agent login --machine-token rlm_0123…\nroll agent up");
    expect(apiFlag("/api")).toBe("");
    expect(machineLabel(pending)).toBe("ada-mbp (macOS)");
    expect(machineLabel({ name: "box", hostname: null, os: null })).toBe("box");
    expect(machineLabel({ name: "box", hostname: "build-1", os: "plan9" })).toBe("build-1 (plan9)");
  });
});
