import { describe, expect, it } from "vitest";
import { windowFromArgv, WINDOW_ARG } from "../shared/bridge.ts";
import { parseDeepLink } from "./deeplink.ts";
import { agentsUrl, delivery, parseAgentsTarget, routeLink, targetLink } from "./windows.ts";

describe("windows", () => {
  it("sends agents, thread and new links to the Agents window and connect links to the main window", () => {
    const route = (raw: string) => routeLink(parseDeepLink(raw)!);
    expect(route("roll://agents")).toEqual({ window: "agents", target: { kind: "canvas" } });
    expect(route("roll://agents/thread/thr_1")).toEqual({ window: "agents", target: { kind: "thread", thread: "thr_1" } });
    expect(route("roll://agents/new")).toEqual({ window: "agents", target: { kind: "new" } });
    expect(route("roll://agents/connect?code=AB-CD")).toEqual({ window: "main", path: "/dashboard/agents/connect?code=AB-CD" });
  });

  it("opens the Agents window on the target's URL", () => {
    expect(agentsUrl("https://roll.sh", { kind: "canvas" })).toBe("https://roll.sh/dashboard/agents");
    expect(agentsUrl("https://roll.sh", { kind: "thread", thread: "thr_1" })).toBe("https://roll.sh/dashboard/agents?focus=t%3Athr_1");
    expect(agentsUrl("http://ai-server.example:3000", { kind: "new" })).toBe("http://ai-server.example:3000/dashboard/agents?new");
  });

  it("hands an open window's page the target as the deep link it already understands", () => {
    expect(targetLink({ kind: "canvas" })).toEqual({ kind: "agents" });
    expect(targetLink({ kind: "thread", thread: "thr_1" })).toEqual({ kind: "thread", thread: "thr_1" });
    expect(targetLink({ kind: "new" })).toEqual({ kind: "new" });
  });

  it("opens, sends, queues until the page listens, or loads", () => {
    expect(delivery({ open: false, listening: false, loading: false })).toBe("open");
    expect(delivery({ open: true, listening: true, loading: false })).toBe("send");
    // A page that listens takes the link even while a subresource is still loading.
    expect(delivery({ open: true, listening: true, loading: true })).toBe("send");
    expect(delivery({ open: true, listening: false, loading: true })).toBe("queue");
    // A loaded page that doesn't listen (the user went elsewhere in the window): load the URL.
    expect(delivery({ open: true, listening: false, loading: false })).toBe("load");
  });

  it("accepts only exact targets from a page", () => {
    expect(parseAgentsTarget({ kind: "canvas" })).toEqual({ kind: "canvas" });
    expect(parseAgentsTarget({ kind: "new", extra: 1 })).toEqual({ kind: "new" });
    expect(parseAgentsTarget({ kind: "thread", thread: "thr_1" })).toEqual({ kind: "thread", thread: "thr_1" });
    for (const raw of [
      null,
      "canvas",
      [],
      {},
      { kind: "connect", code: "AB" },
      { kind: "thread" },
      { kind: "thread", thread: 7 },
      { kind: "thread", thread: "../settings" },
      { kind: "thread", thread: "a".repeat(129) },
    ]) {
      expect(parseAgentsTarget(raw), JSON.stringify(raw)).toBeNull();
    }
  });

  it("tells the preload which window it is in", () => {
    expect(windowFromArgv(["electron", `${WINDOW_ARG}agents`])).toBe("agents");
    expect(windowFromArgv(["electron", `${WINDOW_ARG}main`])).toBe("main");
    expect(windowFromArgv(["electron"])).toBe("main");
    expect(windowFromArgv([`${WINDOW_ARG}agentsx`])).toBe("main");
  });
});
