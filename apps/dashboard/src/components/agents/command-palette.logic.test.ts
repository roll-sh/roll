import { describe, expect, it } from "vitest";
import { machine, project, provider } from "@/test/agents";
import { filterGroups, goalsFirst, nestedItems, reduceStack, type PaletteAction, type PaletteGroup, type PaletteItem } from "./command-palette.logic";
import { goalCommand } from "./orchestrate";

const act = (title: string, searchTerms: string[] = []): PaletteAction => ({ kind: "action", value: `action:${title}`, title, searchTerms, icon: null, run: () => {} });
const thr = (title: string): PaletteAction => ({ ...act(title), value: `thread:${title}`, threadSearch: { title, activityAt: null } });
const titles = (gs: PaletteGroup[]) => gs.map((g) => [g.label, g.items.map((i) => i.title)]);

const projects: PaletteItem = {
  kind: "submenu",
  value: "action:new-in",
  title: "New agent in…",
  icon: null,
  view: { label: "New agent in", placeholder: "", groups: [{ value: "projects", label: "Projects", items: [act("roll"), act("web")] }] },
};
const root: PaletteGroup[] = [
  { value: "actions", label: "Actions", items: [act("Orchestrate…", ["goal", "plan"]), act("New agent", ["launch"]), projects, act("Add machine", ["sandbox"])] },
  { value: "recent", label: "Recent", items: [thr("Fix the flaky queue test")] },
];
const goals = [act("Orchestrate: x"), act("New agent: x")];
const search = (query: string, extra: Partial<Parameters<typeof filterGroups>[0]> = {}) =>
  filterGroups({ groups: root, query, inSubmenu: false, root: { groups: root, nested: nestedItems(root[0].items, query) }, threads: [thr("Fix the flaky queue test"), thr("Upgrade to Next 16.4")], projects: [act("roll"), act("web")], goals: query ? goals : [], ...extra });

describe("the command palette's search", () => {
  it("shows actions and recent threads before typing", () => {
    expect(titles(search(""))).toEqual([
      ["Actions", ["Orchestrate…", "New agent", "New agent in…", "Add machine"]],
      ["Recent", ["Fix the flaky queue test"]],
    ]);
  });

  it("searches actions, submenu options, threads and projects, with the goal rows last for a name", () => {
    expect(titles(search("new"))[0]).toEqual(["Actions", ["New agent", "New agent in…"]]);
    expect(titles(search("upgrade"))).toEqual([
      ["Threads", ["Upgrade to Next 16.4"]],
      ["Ask agents", ["Orchestrate: x", "New agent: x"]],
    ]);
    // A submenu's options are rows of their own ("New agent in web").
    expect(titles(search("new agent in web"))[0]).toEqual(["Suggestions", ["New agent in web"]]);
  });

  it("puts the goal rows first when the query reads like a sentence or nothing matches", () => {
    expect(goalsFirst("roll", 2)).toBe(false);
    expect(goalsFirst("add dark mode to settings", 2)).toBe(true);
    expect(goalsFirst("add dark mode to settings", 3)).toBe(false);
    expect(goalsFirst("zzz", 0)).toBe(true);
    expect(search("zzzz")[0].label).toBe("Ask agents");
  });

  it("limits '>' to actions and offers no goal for it", () => {
    expect(titles(search(">add"))).toEqual([["Actions", ["Add machine"]]]);
  });

  it("searches a submenu, and falls back to everything when nothing in it matches", () => {
    const view = projects.kind === "submenu" ? projects.view : null;
    const inside = (query: string) => filterGroups({ groups: view!.groups!, query, inSubmenu: true, root: { groups: root, nested: [] }, threads: [thr("Upgrade to Next 16.4")] });
    expect(titles(inside("we"))).toEqual([["Projects", ["web"]]]);
    expect(titles(inside("upgrade"))).toEqual([["Threads", ["Upgrade to Next 16.4"]]]);
  });

  it("keeps a stack of views: a submenu starts empty, back restores the query", () => {
    let s = reduceStack({ query: "new in", views: [] }, { type: "push", view: projects.kind === "submenu" ? projects.view : (null as never) });
    expect(s).toMatchObject({ query: "", views: [{ parentQuery: "new in" }] });
    s = reduceStack(s, { type: "back" });
    expect(s).toEqual({ query: "new in", views: [] });
  });
});

describe("a goal from outside a conversation", () => {
  const m = machine({ info: { ...machine().info!, providers: [provider({ auth: "missing" }), provider({ id: "codex", name: "Codex", models: [{ id: "gpt-5.5-codex", name: "GPT-5.5 Codex", default: true, efforts: [] }] })] } });
  const target = { project: { ...project(), machine: m.id }, machine: m };

  it("starts an orchestrator: full access, default mode, in the checkout, on a logged-in provider", () => {
    expect(goalCommand("orchestrate", "  Add dark mode  ", target)).toEqual({
      type: "thread.create",
      project: "prj_1",
      provider: "codex",
      model: "gpt-5.5-codex",
      runtime_mode: "full_access",
      interaction_mode: "default",
      worktree: false,
      prompt: "Add dark mode",
      orchestrator: true,
    });
  });

  it("starts one agent from the project's defaults", () => {
    const p = { ...target.project, repo: { owner: "acme", repo: "roll" }, settings: { auto_pull: false, scripts: [], defaults: { provider: "claude", model: "claude-sonnet-5", effort: "high", runtime_mode: "auto_edit" as const, interaction_mode: "plan" as const } } };
    expect(goalCommand("agent", "fix it", { ...target, project: p })).toEqual({
      type: "thread.create",
      project: "prj_1",
      provider: "claude",
      model: "claude-sonnet-5",
      effort: "high",
      runtime_mode: "auto_edit",
      interaction_mode: "plan",
      worktree: true,
      prompt: "fix it",
    });
  });

  it("says why it can't start", () => {
    expect(goalCommand("orchestrate", " ", target)).toEqual({ error: "Say what should get done." });
    expect(goalCommand("orchestrate", "x", { ...target, machine: { ...m, online: false } })).toEqual({ error: "ada-mbp is offline." });
    expect(goalCommand("agent", "x", { ...target, machine: { ...m, info: { ...m.info!, providers: [] } } })).toEqual({ error: "No agent is installed on ada-mbp." });
  });
});
