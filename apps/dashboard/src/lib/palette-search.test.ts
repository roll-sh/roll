import { describe, expect, it } from "vitest";
import { foldSearchQuery, rankSearchGroups, recordThreadOpen, scoreSearchTerms, scoreThreadSearch, splitSearchText, THREAD_OPEN_DEDUPE_MS, type ThreadSearchSubject } from "./palette-search";

const NOW = Date.UTC(2026, 10, 4);
const DAY = 24 * 60 * 60 * 1000;
const subject = (title: string, over: Partial<ThreadSearchSubject> = {}): ThreadSearchSubject => ({ title, activityAt: null, ...over });
const q = (s: string) => foldSearchQuery(s);

describe("palette search (T3's ranking)", () => {
  it("folds case, accents, quotes and spaces", () => {
    expect(q('  "Café   Déjà"  ')).toBe("cafe deja");
  });

  it("matches at word starts, camelCase and joined words, and inside a word only for long tokens in titles", () => {
    expect(scoreSearchTerms(q("queue"), ["Fix the flaky queue test"])?.quality).toBe(1);
    expect(scoreSearchTerms(q("json"), ["parseJSONSchema"])).not.toBeNull();
    expect(scoreSearchTerms(q("vscode"), ["Open in VS Code"])).not.toBeNull();
    // "del" doesn't find "model"; a title lets four letters match inside a word.
    expect(scoreSearchTerms(q("del"), ["Pick a model"])).toBeNull();
    expect(scoreThreadSearch(q("aky"), subject("Fix the flaky test"), NOW)).toBeNull();
    expect(scoreThreadSearch(q("laky"), subject("Fix the flaky test"), NOW)?.quality).toBeCloseTo(0.3);
  });

  it("drops a leading verb a command doesn't hold ('open the shelf' names the shelf)", () => {
    expect(scoreSearchTerms(q("open the shelf"), ["Shelf", "settled"])?.strong).toBe(true);
    expect(scoreSearchTerms(q("go to settings"), ["Agents settings"])).not.toBeNull();
    // A verb the command holds still counts, and the rest must still match.
    expect(scoreSearchTerms(q("open in vs code"), ["Open in VS Code"])?.ranges[0]).toEqual({ start: 0, end: 4 });
    expect(scoreSearchTerms(q("open the fridge"), ["Shelf"])).toBeNull();
    expect(scoreSearchTerms(q("open"), ["Shelf"])).toBeNull();
  });

  it("forgives a typo only when nothing matches strongly", () => {
    const groups = [{ group: "threads", items: [subject("Upgrade to Next 16.4"), subject("Fix the flaky queue test")] }];
    const typo = rankSearchGroups(groups, (t, o) => scoreThreadSearch(q("upgarde"), t, NOW, o));
    expect(typo[0].ranked.map((r) => r.item.title)).toEqual(["Upgrade to Next 16.4"]);
    // With a real match, typos aren't tried at all.
    const exact = rankSearchGroups(groups, (t, o) => scoreThreadSearch(q("queue"), t, NOW, o));
    expect(exact[0].ranked.map((r) => r.item.title)).toEqual(["Fix the flaky queue test"]);
  });

  it("puts an action before a thread of the same class, so 'new' runs New agent", () => {
    const ranked = rankSearchGroups<string, string, NonNullable<ReturnType<typeof scoreSearchTerms>>>(
      [
        { group: "actions", items: ["New agent"] },
        { group: "threads", items: ["New onboarding flow"] },
      ],
      (t, o) => scoreSearchTerms(q("new"), [t], o),
    );
    expect(ranked.map((g) => g.group)).toEqual(["actions", "threads"]);
  });

  it("explains a match outside the title: the change, the branch, the project", () => {
    const t = subject("Make claims atomic", { change: { number: 41, repo: "acme/roll" }, branch: "roll/claim-race", projectTitle: "roll" });
    expect(scoreThreadSearch(q("#41"), t, NOW)?.explain?.()).toMatchObject({ label: "Change" });
    expect(scoreThreadSearch(q("roll/claim-race"), t, NOW)?.explain?.()).toMatchObject({ label: "Branch", text: "claim-race" });
    // roll's own worktree prefix alone matches nothing extra.
    expect(scoreThreadSearch(q("roll"), subject("Something else", { branch: "roll/x" }), NOW)).toBeNull();
  });

  it("lets use reorder matches of one class: recent activity, opens, and what was picked for the query", () => {
    const quiet = subject("Fix the queue", { activityAt: NOW - 60 * DAY });
    const busy = subject("Fix the queue later", { activityAt: NOW - DAY });
    const items = [{ group: "threads", items: [quiet, busy] }];
    const order = (query: string) => rankSearchGroups(items, (t, o) => scoreThreadSearch(q(query), t, NOW, o))[0].ranked.map((r) => r.item.title);
    expect(order("queue")).toEqual(["Fix the queue later", "Fix the queue"]);
    // Picked for "que" yesterday: first for that query and the ones it starts.
    const picked = { ...quiet, opens: recordThreadOpen(null, NOW - DAY, "queue") };
    const withPick = [{ group: "threads", items: [picked, busy] }];
    expect(rankSearchGroups(withPick, (t, o) => scoreThreadSearch(q("queue"), t, NOW, o))[0].ranked[0].item).toBe(picked);
  });

  it("counts opens close together once, and keeps the last three picks", () => {
    let s = recordThreadOpen(null, NOW, "a");
    s = recordThreadOpen(s, NOW + 1000, "bbb");
    expect(s.score).toBe(1);
    s = recordThreadOpen(s, NOW + THREAD_OPEN_DEDUPE_MS + 1, "ccc");
    s = recordThreadOpen(s, NOW + 2 * THREAD_OPEN_DEDUPE_MS + 2, "ddd");
    expect(s.score).toBeGreaterThan(2.9);
    expect(s.picks.map((p) => p.query)).toEqual(["ddd", "ccc", "bbb"]);
  });

  it("cuts text at highlight ranges", () => {
    expect(splitSearchText("claim-race", [{ start: 0, end: 5 }])).toEqual([
      { text: "claim", highlighted: true, start: 0 },
      { text: "-race", highlighted: false, start: 5 },
    ]);
  });
});
