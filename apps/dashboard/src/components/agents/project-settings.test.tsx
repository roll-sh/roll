import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { agentsSurface, resetAgentsSurface } from "@/lib/agents-surface";
import type { Command, Project, Reply } from "@/lib/agents-api";
import { NO_SETTINGS } from "@/lib/agents-api";
import { Composer } from "./composer";
import { ProjectNode } from "./nodes";
import { NewProject } from "./project-new";
import { ProjectSettingsForm, pullText } from "./project-settings";
import { SettingsPanel } from "./settings-panel";
import { machine, mirrored, project, provider, T0 } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";

const updates = (h: ReturnType<typeof agentsHarness>) => h.command.mock.calls.map((c) => c[2]).filter((c): c is Extract<Command, { type: "project.update" }> => c.type === "project.update");

const withSettings = (over: Partial<Project["settings"]> = {}, rest: Partial<Project> = {}) =>
  project({ git: { branch: "main", head: "f35f34cf", remote: null, dirty: false }, settings: { ...NO_SETTINGS, ...over }, ...rest });

describe("pullText", () => {
  const at = T0 - 6 * 60_000;
  it("says what the last pull did, calmly", () => {
    expect(pullText(null)).toBeNull();
    expect(pullText({ at, outcome: { status: "pulled", branch: "main", from: "a", to: "b", commits: 3 } }, T0)).toEqual({ text: "Pulled 3 commits into main · 6 min ago", tone: "ok" });
    expect(pullText({ at, outcome: { status: "pulled", branch: "main", from: "a", to: "b", commits: 1 } }, T0)?.text).toMatch(/^Pulled 1 commit into/);
    expect(pullText({ at, outcome: { status: "up_to_date", branch: "main" } }, T0)?.text).toBe("main is up to date · 6 min ago");
    expect(pullText({ at, outcome: { status: "skipped", reason: "local_changes", message: "the checkout has uncommitted changes" } }, T0)).toEqual({
      text: "Didn't pull: the checkout has uncommitted changes · 6 min ago",
      tone: "skip",
    });
    expect(pullText({ at, outcome: { status: "failed", message: "fetching origin: no route" } }, T0)?.tone).toBe("fail");
  });
});

/** Opens the select named `label` and picks `option` (T3's selects, not native ones). */
async function pick(label: string, option: string) {
  await userEvent.click(screen.getByRole("combobox", { name: label }));
  await userEvent.click(await screen.findByRole("option", { name: option }));
}

/** Opens the model picker and picks a model. */
async function pickModel(name: RegExp) {
  await userEvent.click(screen.getByRole("button", { name: /^Model:/ }));
  await userEvent.click(await screen.findByRole("option", { name }));
}

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

  it("keeps an unsaved change in view at the bottom, with Discard", async () => {
    const h = agentsHarness();
    renderAgents(<ProjectSettingsForm project={withSettings({})} machine={machine()} />, h);
    expect(screen.queryByRole("region", { name: "Unsaved changes" })).toBeNull();
    const toggle = screen.getByRole("switch", { name: "Pull merged changes automatically" });
    await userEvent.click(toggle);
    const bar = screen.getByRole("region", { name: "Unsaved changes" });
    expect(bar.className).toContain("sticky");
    await userEvent.click(within(bar).getByRole("button", { name: "Discard" }));
    expect(toggle).toHaveAttribute("aria-checked", "false");
    expect(screen.queryByRole("region", { name: "Unsaved changes" })).toBeNull();
    expect(updates(h)).toEqual([]);
  });

  it("sets what new threads start with, and a change back is no change", async () => {
    const h = agentsHarness();
    const m = machine({ info: { ...machine().info!, providers: [provider(), provider({ id: "codex", name: "Codex", models: [] })] } });
    renderAgents(<ProjectSettingsForm project={withSettings()} machine={m} />, h);
    const save = screen.getByRole("button", { name: "Save" });
    const workspace = () => within(screen.getByRole("group", { name: "Where new threads work" }));
    await userEvent.click(workspace().getByRole("button", { name: "Worktree" }));
    expect(save).toBeEnabled();
    await userEvent.click(workspace().getByRole("button", { name: "Default" }));
    expect(save).toBeDisabled();

    // The model comes from T3's model picker; its reset forgets provider, model and effort.
    await pickModel(/Sonnet 5/);
    expect(screen.getByRole("button", { name: /^Model: Sonnet 5/ })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Reset to default: Model" }));
    expect(save).toBeDisabled();
    await pickModel(/Sonnet 5/);
    await userEvent.click(workspace().getByRole("button", { name: "Checkout" }));
    await pick("Permissions for new threads", "Ask");
    await userEvent.click(within(screen.getByRole("group", { name: "Interaction mode for new threads" })).getByRole("button", { name: "Plan first" }));
    await userEvent.click(save);
    await waitFor(() => expect(updates(h)).toHaveLength(1));
    expect(updates(h)[0].settings?.defaults).toEqual({ provider: "claude", model: "claude-sonnet-5", worktree: false, runtime_mode: "ask", interaction_mode: "plan" });
  });

  it("edits scripts: one setup script, saved ones run in a terminal", async () => {
    const reply = (c: Command): Reply => (c.type === "terminal.open" ? { type: "created", id: "trm_1" } : { type: "ok" });
    const h = agentsHarness({ reply });
    const p = withSettings({ scripts: [{ id: "lint", name: "Lint", command: "pnpm lint", icon: "lint", on_worktree: true }] });
    renderAgents(<ProjectSettingsForm project={p} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Run Lint" }));
    await waitFor(() => expect(h.command.mock.calls.map((c) => c[2])).toContainEqual({ type: "terminal.open", project: "prj_1", script: "lint", cols: 100, rows: 26 }));

    await userEvent.click(screen.getByRole("button", { name: "Add a script" }));
    const save = screen.getByRole("button", { name: "Save" });
    // A script needs a name and a command.
    expect(save).toBeDisabled();
    const names = screen.getAllByRole("textbox", { name: "Script name" });
    const commands = screen.getAllByRole("textbox", { name: "Script command" });
    await userEvent.type(names[1], "Install");
    await userEvent.type(commands[1], "pnpm install");
    await userEvent.click(screen.getAllByRole("combobox", { name: "Script icon" })[1]);
    await userEvent.click(await screen.findByRole("option", { name: "Setup" }));
    // A new script can't run before it's saved.
    expect(screen.queryByRole("button", { name: "Run Install" })).not.toBeInTheDocument();
    const setup = screen.getAllByRole("switch", { name: "Run in each new worktree (setup)" });
    await userEvent.click(setup[1]);
    expect(setup[0]).not.toBeChecked();
    expect(setup[1]).toBeChecked();
    await userEvent.click(save);
    await waitFor(() => expect(updates(h)).toHaveLength(1));
    const scripts = updates(h)[0].settings?.scripts ?? [];
    expect(scripts.map(({ id: _id, ...s }) => s)).toEqual([
      { name: "Lint", command: "pnpm lint", icon: "lint", on_worktree: false },
      { name: "Install", command: "pnpm install", icon: "configure", on_worktree: true },
    ]);
    expect(scripts[1].id).toMatch(/^[A-Za-z0-9_-]+$/);

    await userEvent.click(screen.getByRole("button", { name: "Remove Lint" }));
    expect(screen.getAllByRole("textbox", { name: "Script name" })).toHaveLength(1);
  });

  it("pulls on demand, shows the last pull, and says why a save failed", async () => {
    const reply = (c: Command): Reply =>
      c.type === "project.pull"
        ? { type: "pulled", at: T0, outcome: { status: "up_to_date", branch: "main" } }
        : { type: "error", code: "invalid", message: "script id \"x y\" must be 1 to 64 letters" };
    const h = agentsHarness({ reply });
    const p = withSettings({ auto_pull: true }, { pull: { at: Date.now() - 120_000, outcome: { status: "skipped", reason: "local_changes", message: "the checkout has uncommitted changes" } } });
    renderAgents(<ProjectSettingsForm project={p} machine={machine()} />, h);
    expect(screen.getByText(/Didn't pull: the checkout has uncommitted changes · 2 min ago/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Pull now" }));
    await waitFor(() => expect(h.command.mock.calls.map((c) => c[2])).toContainEqual({ type: "project.pull", project: "prj_1" }));
    await userEvent.clear(screen.getByRole("textbox", { name: "Name" }));
    await userEvent.type(screen.getByRole("textbox", { name: "Name" }), "Roll");
    await userEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByText(/must be 1 to 64 letters/)).toBeInTheDocument();
    expect(updates(h)[0]).toMatchObject({ name: "Roll" });
  });

  it("can't save while the machine is offline", () => {
    renderAgents(<ProjectSettingsForm project={withSettings()} machine={machine({ online: false })} />, agentsHarness());
    expect(screen.getByText("ada-mbp is offline")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Pull now" })).toBeDisabled();
  });
});

describe("project settings on the canvas", () => {
  beforeEach(() => resetAgentsSurface());

  it("open from the project card's menu in the side panel beside the canvas", async () => {
    const p = withSettings({ auto_pull: true });
    const h = agentsHarness({ entities: [mirrored("project", p, 1)] });
    renderAgents(<ProjectNode project={p} machine={machine()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Project" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Settings" }));
    expect(agentsSurface.get().view).toEqual({ kind: "settings", target: `p:${p.id}` });
    const onClose = vi.fn();
    renderAgents(<SettingsPanel target={`p:${p.id}`} onClose={onClose} />, h);
    expect(screen.getByText("Project settings")).toBeInTheDocument();
    expect(screen.getByRole("switch", { name: "Pull merged changes automatically" })).toHaveAttribute("aria-checked", "true");
    await userEvent.click(screen.getByRole("button", { name: "Close (Esc)" }));
    expect(onClose).toHaveBeenCalled();
  });

  it("are offered while adding a project: on for a clone, off for a folder", async () => {
    const reply = (c: Command): Reply => (c.type === "fs.list" ? { type: "entries", entries: [] } : c.type === "project.add" ? { type: "created", id: "prj_new" } : { type: "ok" });
    const repos = [{ owner: "acme", name: "roll", full_name: "acme/roll", description: null, visibility: "public" as const, updated_at: 1 }];
    const h = agentsHarness({ reply, ctx: { services: fakeServices({ repos: vi.fn(async () => repos) }) } });
    const onAdded = vi.fn();
    renderAgents(<NewProject machine={machine()} onClose={vi.fn()} onAdded={onAdded} />, h);
    const offer = screen.getByRole("switch", { name: /Pull merged changes/ });
    expect(offer).not.toBeChecked();
    await userEvent.click(screen.getByRole("tab", { name: "Clone a repo" }));
    expect(offer).toBeChecked();
    await userEvent.click(within(await screen.findByRole("list", { name: "Repos" })).getByRole("button", { name: /^roll/ }));
    await waitFor(() => expect(onAdded).toHaveBeenCalledWith("prj_new"));
    expect(updates(h)).toEqual([{ type: "project.update", project: "prj_new", settings: { auto_pull: true, defaults: {}, scripts: [], auto_settle: { after_days: 3, on_land: true } } }]);
  });

  it("an add with the offer turned off sends no settings", async () => {
    const reply = (c: Command): Reply => (c.type === "fs.list" ? { type: "entries", entries: [] } : { type: "created", id: "prj_new" });
    const h = agentsHarness({ reply, ctx: { services: fakeServices() } });
    const onAdded = vi.fn();
    renderAgents(<NewProject machine={machine()} onClose={vi.fn()} onAdded={onAdded} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Add this folder" }));
    await waitFor(() => expect(onAdded).toHaveBeenCalledWith("prj_new"));
    expect(updates(h)).toEqual([]);
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
