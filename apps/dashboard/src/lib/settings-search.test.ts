import { describe, expect, it } from "vitest";
import { normalizeSearch, searchSettings, type SettingItem } from "./settings-search";

const items: SettingItem[] = [
  { id: "notify", title: "Notifications", section: "General", terms: ["sound", "alert when an agent needs you"] },
  { id: "binary", title: "Binary path", section: "Codex", terms: ["executable", "command"] },
  { id: "env", title: "Environment", section: "Codex", terms: ["variables", "CODEX_HOME"] },
  { id: "worktrees", title: "Worktree folder", section: "Worktrees", terms: ["where new worktrees go"] },
  { id: "branch", title: "Branch prefix", section: "Worktrees" },
  { id: "notes", title: "Release notes", section: "About" },
];

describe("searchSettings (T3's settingsSearch)", () => {
  it("normalizes case, accents and spaces", () => {
    expect(normalizeSearch("  Ünïcode   Thing ")).toBe("unicode thing");
  });

  it("finds nothing for an empty query", () => {
    expect(searchSettings("   ", items)).toEqual([]);
  });

  it("needs every word somewhere: the title, the section or a term", () => {
    expect(searchSettings("codex env", items).map((i) => i.id)).toEqual(["env"]);
    expect(searchSettings("codex_home", items).map((i) => i.id)).toEqual(["env"]);
    expect(searchSettings("codex nothing", items)).toEqual([]);
    expect(searchSettings("SOUND", items).map((i) => i.id)).toEqual(["notify"]);
  });

  it("ranks the title first: exact, prefix, contains, every word, then the phrase elsewhere", () => {
    const ranked: SettingItem[] = [
      { id: "phrase", title: "Other", section: "S", terms: ["work tree"] },
      { id: "words", title: "Tree of work", section: "S" },
      { id: "contains", title: "The work tree", section: "S" },
      { id: "prefix", title: "Work tree folder", section: "S" },
      { id: "exact", title: "Work tree", section: "S" },
    ];
    expect(searchSettings("work tree", ranked).map((i) => i.id)).toEqual(["exact", "prefix", "contains", "words", "phrase"]);
  });

  it("keeps the panel's order between equals", () => {
    expect(searchSettings("worktrees", items).map((i) => i.id)).toEqual(["worktrees", "branch"]);
    expect(searchSettings("no", items).map((i) => i.id)).toEqual(["notify", "notes"]);
  });
});
