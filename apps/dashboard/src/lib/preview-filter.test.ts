import { describe, expect, it } from "vitest";
import { counts, matches } from "./preview-filter";
import type { Preview } from "./previews-api";

const p = (over: Partial<Preview>) =>
  ({ id: "pv_1", title: "Faster cart", branch: "feat/cart", slug: "pr12-shop", repo: "acme/web", number: 12, github_pr: null, state: "ready", desired: "active", visual_review: null, ...over }) as unknown as Preview;

describe("the Previews list's filter", () => {
  it("matches every word against title, branch, slug, repo and #number, and filters by state", () => {
    const list = [
      p({}),
      p({ id: "pv_2", title: "Docs", branch: "docs/setup", slug: "c13-shop", number: 13, state: "sleeping" }),
      p({ id: "pv_3", title: "Fork's badge", branch: "pull/7", slug: "pr7-shop", repo: "git:svc_shop", number: null, state: "awaiting_approval" }),
      p({ id: "pv_4", title: "Broken", branch: "feat/x", slug: "c14-shop", number: 14, state: "failed", visual_review: { state: "changed" } as Preview["visual_review"] }),
    ];
    const shown = (text: string, show: Parameters<typeof matches>[2]) => list.filter((x) => matches(x, text, show)).map((x) => x.id);
    expect(shown("cart", "all")).toEqual(["pv_1"]);
    expect(shown("#13", "all")).toEqual(["pv_2"]);
    expect(shown("ACME web", "all")).toEqual(["pv_1", "pv_2", "pv_4"]);
    expect(shown("", "you")).toEqual(["pv_3", "pv_4"]);
    expect(shown("", "asleep")).toEqual(["pv_2"]);
    expect(shown("docs", "failed")).toEqual([]);
    expect(counts(list)).toEqual({ all: 4, you: 2, failed: 1, awake: 1, asleep: 1 });
  });
});
