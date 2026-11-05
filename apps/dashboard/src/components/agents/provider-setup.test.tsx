import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Machine } from "@/lib/agents-api";
import { notReadyReason, readiness } from "@/lib/agents-setup";
import { agentsSurface, resetAgentsSurface } from "@/lib/agents-surface";
import { NO_CANVAS } from "./machine-projects";
import { Launcher } from "./new-agent";
import { MachineNode, ProjectNode } from "./nodes";
import { projectActions } from "./project-actions";
import { machine, mirrored, project, provider } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";

const withProviders = (providers: ReturnType<typeof provider>[], over: Partial<Machine> = {}): Machine => machine({ info: { ...machine().info!, providers }, ...over });
const BARE = withProviders([]);
const LOGGED_OUT = withProviders([provider({ auth: "missing" }), provider({ id: "codex", name: "Codex", auth: "missing" })]);

afterEach(() => resetAgentsSurface());

describe("whether an agent can run on a machine", () => {
  it("needs a provider installed and logged in (or not known to be logged out)", () => {
    expect(readiness(machine()).state).toBe("ready");
    expect(readiness(withProviders([provider({ auth: "unknown" })])).state).toBe("ready");
    expect(readiness(BARE).state).toBe("none");
    expect(readiness(withProviders([provider({ installed: false, auth: "missing" })])).state).toBe("none");
    expect(readiness(LOGGED_OUT)).toMatchObject({ state: "login", missing: [expect.objectContaining({ id: "claude" }), expect.objectContaining({ id: "codex" })] });
    expect(readiness(withProviders([provider({ installed: false, checking: true })])).state).toBe("checking");
    expect(readiness(machine({ online: false })).state).toBe("offline");
    expect(readiness(machine({ info: null })).state).toBe("connecting");
    expect(notReadyReason(BARE)).toBe("No agent provider on ada-mbp yet");
    expect(notReadyReason(LOGGED_OUT)).toBe("No provider is logged in on ada-mbp");
  });
});

describe("a machine with no provider", () => {
  it("says no agent can run there, and the next steps come before adding a project", async () => {
    const h = agentsHarness({ machines: [BARE] });
    renderAgents(<MachineNode machine={BARE} />, h);
    const setup = screen.getByRole("group", { name: "Set up a provider" });
    expect(setup).toHaveTextContent("No agent can run on ada-mbp yet: it has no provider.");
    // Adding a project stays possible, but isn't the obvious step.
    const add = screen.getAllByRole("button", { name: /Add a project/ }).find((b) => b.className.includes("w-full"))!;
    expect(add.className).not.toContain("bg-fg");
    await userEvent.click(within(setup).getByRole("button", { name: /check again/i }));
    expect(h.command).toHaveBeenLastCalledWith("mch_1", expect.any(String), { type: "provider.refresh" });
    await userEvent.click(within(setup).getByRole("button", { name: "Add from the ACP registry" }));
    expect(agentsSurface.get().view).toEqual({ kind: "settings", target: "m:mch_1" });
    resetAgentsSurface();
    await userEvent.click(within(setup).getByRole("button", { name: "Use an endpoint or gateway" }));
    expect(agentsSurface.get().view).toEqual({ kind: "settings", target: "m:mch_1" });
  });

  it("a computer with providers logged out gets their logins (not only sandboxes)", async () => {
    const h = agentsHarness({ machines: [LOGGED_OUT], reply: () => ({ type: "created", id: "trm_1" }) });
    renderAgents(<MachineNode machine={LOGGED_OUT} />, h);
    const login = screen.getByRole("group", { name: "Log in to a provider" });
    await userEvent.click(within(login).getByRole("button", { name: "Log in to Claude Code" }));
    expect(h.command).toHaveBeenCalledWith("mch_1", expect.any(String), { type: "terminal.open", command: "claude /login", cols: 100, rows: 26 });
    await waitFor(() => expect(h.focus).toHaveBeenCalledWith("x:trm_1"));
  });

  it("its projects can't start an agent: the card says why and where to fix it", async () => {
    const startNew = vi.fn();
    const h = agentsHarness({ machines: [BARE], entities: [mirrored("project", project(), 1)], ctx: { startNew } });
    renderAgents(<ProjectNode project={project()} machine={BARE} />, h);
    expect(screen.queryByRole("button", { name: "Start an agent" })).not.toBeInTheDocument();
    expect(screen.getByText(/No agent provider on ada-mbp yet/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "No agent provider on ada-mbp yet" })).toBeDisabled();
    await userEvent.click(screen.getByRole("button", { name: "Set one up" }));
    expect(agentsSurface.get().view).toEqual({ kind: "settings", target: "m:mch_1" });
    // Its menu too: New agent here is off, the setup step is right under it.
    const labels = projectActions({ ...h.ctx, canvas: NO_CANVAS }, project()).map((a) => [a.label, !!a.disabled]);
    expect(labels.slice(0, 2)).toEqual([
      ["New agent here", true],
      ["Set up a provider on ada-mbp…", false],
    ]);
    expect(startNew).not.toHaveBeenCalled();
  });

  it("the launcher shows the steps instead of a composer that can't run", () => {
    const h = agentsHarness({ machines: [LOGGED_OUT], entities: [mirrored("project", project(), 1)] });
    renderAgents(<Launcher onClose={vi.fn()} />, h);
    const login = screen.getByRole("group", { name: "Log in to a provider" });
    expect(within(login).getByRole("button", { name: "Log in to Codex" })).toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "Message" })).not.toBeInTheDocument();
  });
});
