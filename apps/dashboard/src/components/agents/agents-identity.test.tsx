import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { type Command, type Project } from "@/lib/agents-api";
import { ProjectSettingsForm } from "./project-settings";
import { SettingsPanel } from "./settings-panel";
import { machine, machineConfig, project } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";

const updates = (h: ReturnType<typeof agentsHarness>) => h.command.mock.calls.map((c) => c[2]).filter((c): c is Extract<Command, { type: "project.update" }> => c.type === "project.update");

describe("project settings: colour and icon", () => {
  const withGit = (over: Partial<Project> = {}) => project({ git: { branch: "main", head: "f35f34cf", remote: null, dirty: false }, ...over });

  it("picks a colour and an emoji and saves them with the settings", async () => {
    const h = agentsHarness();
    renderAgents(<ProjectSettingsForm project={withGit()} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("radio", { name: "Rose" }));
    await userEvent.click(screen.getByRole("combobox", { name: "How the icon is made" }));
    await userEvent.click(await screen.findByRole("option", { name: "Emoji" }));
    await userEvent.type(screen.getByRole("textbox", { name: "Emoji" }), "🦀");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(updates(h)).toHaveLength(1));
    expect(updates(h)[0].settings).toMatchObject({ color: "rose", icon: { kind: "emoji", emoji: "🦀" } });
  });
});

describe("machine settings: colour and kind", () => {
  it("sends the colour and kind to the relay, and Auto clears them; works while the machine is offline", async () => {
    const services = fakeServices();
    const m = machine({ online: false, appearance: { color: "pink" }, info: { ...machine().info!, settings: machineConfig() } });
    const h = agentsHarness({ machines: [m], ctx: { services } });
    renderAgents(<SettingsPanel target="m:mch_1" onClose={vi.fn()} />, h);
    expect(screen.getByRole("radio", { name: "Pink" })).toHaveAttribute("aria-checked", "true");
    await userEvent.click(screen.getByRole("radio", { name: "Teal" }));
    await waitFor(() => expect(services.machines.style).toHaveBeenCalledWith("acme", "mch_1", { color: "teal" }));
    await userEvent.click(screen.getByRole("radio", { name: "Server" }));
    await waitFor(() => expect(services.machines.style).toHaveBeenCalledWith("acme", "mch_1", { kind: "server" }));
    await userEvent.click(within(screen.getByRole("radiogroup", { name: "Colour" })).getByRole("radio", { name: /^Auto/ }));
    await waitFor(() => expect(services.machines.style).toHaveBeenCalledWith("acme", "mch_1", { color: null }));
  });
});
