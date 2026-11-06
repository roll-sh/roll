import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { resetAgentsSurface } from "@/lib/agents-surface";
import type { Command, Project } from "@/lib/agents-api";
import { NO_SETTINGS } from "@/lib/agents-api";
import { Composer } from "./composer";
import { ProjectSettingsForm } from "./project-settings";
import { SettingsPanel } from "./settings-panel";
import { machine, mirrored, project, provider } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";

const updates = (h: ReturnType<typeof agentsHarness>) => h.command.mock.calls.map((c) => c[2]).filter((c): c is Extract<Command, { type: "project.update" }> => c.type === "project.update");

const withSettings = (over: Partial<Project["settings"]> = {}, rest: Partial<Project> = {}) =>
  project({ git: { branch: "main", head: "f35f34cf", remote: null, dirty: false }, settings: { ...NO_SETTINGS, ...over }, ...rest });

describe("ProjectSettingsForm", () => {
  it("turns auto-pull on and saves the settings whole", async () => {
    const h = agentsHarness();
    const onDone = vi.fn();
    const p = withSettings({ scripts: [{ id: "test", name: "Test", command: "cargo test", icon: "test", on_worktree: false }] });
    renderAgents(<ProjectSettingsForm project={p} machine={machine()} onDone={onDone} />, h);
    const save = screen.getByRole("button", { name: "Save" });
    expect(save).toBeDisabled();
    const toggle = screen.getByRole("switch", { name: "Pull merged changes automatically" });
    expect(toggle).toHaveAttribute("aria-checked", "false");
    expect(screen.getByText(/only while it's on main with no local changes/)).toBeInTheDocument();
    await userEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-checked", "true");
    await userEvent.click(save);
    await waitFor(() => expect(onDone).toHaveBeenCalled());
    expect(updates(h)).toEqual([
      {
        type: "project.update",
        project: "prj_1",
        settings: { auto_pull: true, defaults: {}, scripts: [{ id: "test", name: "Test", command: "cargo test", icon: "test", on_worktree: false }], auto_settle: { after_days: 3, on_land: true } },
      },
    ]);
  });

});

describe("project settings on the canvas", () => {
  beforeEach(() => resetAgentsSurface());

  it("won't close over an unsaved change without saying so: once refused, then Discard closes it", async () => {
    const p = withSettings({});
    const h = agentsHarness({ entities: [mirrored("project", p, 1)] });
    const onClose = vi.fn();
    renderAgents(<SettingsPanel target={`p:${p.id}`} onClose={onClose} />, h);
    await userEvent.click(screen.getByRole("switch", { name: "Pull merged changes automatically" }));
    await userEvent.click(screen.getByRole("button", { name: "Close (Esc)" }));
    expect(onClose).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toHaveTextContent("Save or discard your changes first");
    await userEvent.click(screen.getByRole("button", { name: "Discard" }));
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
    expect(screen.getByRole("switch", { name: "Pull merged changes automatically" })).toHaveAttribute("aria-checked", "false");
  });

  it("new threads start from the project's defaults", async () => {
    const onSubmit = vi.fn(async () => {});
    const m = machine({ info: { ...machine().info!, providers: [provider(), provider({ id: "codex", name: "Codex", models: [] })] } });
    const p = withSettings({ defaults: { provider: "codex", runtime_mode: "ask", interaction_mode: "plan", worktree: true } });
    renderAgents(<Composer mode={{ kind: "new", project: p, machine: m }} onSubmit={onSubmit} />, agentsHarness());
    expect(screen.getByRole("button", { name: /^Model: Codex/ })).toBeInTheDocument();
    expect(screen.getByRole("switch", { name: "Worktree" })).toBeChecked();
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "go{Enter}");
    await waitFor(() => expect(onSubmit).toHaveBeenCalledWith(expect.objectContaining({ provider: "codex", runtime_mode: "ask", interaction_mode: "plan", worktree: true })));
  });
});
