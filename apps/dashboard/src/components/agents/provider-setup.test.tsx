import { screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Machine } from "@/lib/agents-api";
import { notReadyReason, readiness } from "@/lib/agents-setup";
import { resetAgentsSurface } from "@/lib/agents-surface";
import { Launcher } from "./new-agent";
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
  it("the launcher shows the steps instead of a composer that can't run", () => {
    const h = agentsHarness({ machines: [LOGGED_OUT], entities: [mirrored("project", project(), 1)] });
    renderAgents(<Launcher onClose={vi.fn()} />, h);
    const login = screen.getByRole("group", { name: "Log in to a provider" });
    expect(within(login).getByRole("button", { name: "Log in to Codex" })).toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "Message" })).not.toBeInTheDocument();
  });
});
