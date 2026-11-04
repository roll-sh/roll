import { afterEach, describe, expect, it, vi } from "vitest";
import type { Machine, Request, Thread } from "./agents-api";
import { desktop, desktopStatus, onDesktopDeepLink, openAgentsWindow, reportToDesktop, type RollDesktop } from "./desktop";

const thread = (id: string, status: Thread["status"], title = id, archived_at: number | null = null) =>
  ({ id, title, status, archived_at }) as Thread;

const approval = (id: string, threadId: string, created_at: number, command: string | null, status: Request["status"] = "pending") =>
  ({
    id,
    thread: threadId,
    status,
    created_at,
    body: { type: "approval", action: "command", title: "Run a command", command, decisions: [] },
  }) as unknown as Request;

const question = (id: string, threadId: string, created_at: number, text: string) =>
  ({
    id,
    thread: threadId,
    status: "pending",
    created_at,
    body: { type: "questions", questions: [{ id: "q", header: "", question: text, options: [], multi: false }] },
  }) as unknown as Request;

describe("desktopStatus", () => {
  it("counts live running threads and lists pending requests oldest first", () => {
    const s = desktopStatus(
      [thread("t1", "running", "Fix the build"), thread("t2", "starting"), thread("t3", "idle"), thread("t4", "running", "old", 5)],
      [
        question("r2", "t2", 20, "Which branch?"),
        approval("r1", "t1", 10, "cargo test"),
        approval("r3", "t1", 30, null),
        approval("r4", "t1", 5, "rm -rf target", "resolved"),
      ],
      [{ id: "m1", online: true } as Machine, { id: "m2", online: false } as Machine],
    );
    expect(s.running).toBe(2);
    expect(s.pending).toEqual([
      { thread: "t1", title: "Fix the build", request: "r1", kind: "approval", summary: "cargo test" },
      { thread: "t2", title: "t2", request: "r2", kind: "question", summary: "Which branch?" },
      { thread: "t1", title: "Fix the build", request: "r3", kind: "approval", summary: "Run a command" },
    ]);
    expect(s.machines).toEqual([
      { id: "m1", online: true },
      { id: "m2", online: false },
    ]);
  });
});

describe("bridge helpers", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("do nothing without the desktop app", () => {
    expect(desktop()).toBeNull();
    reportToDesktop({ running: 0, pending: [] });
    expect(onDesktopDeepLink(() => {})).toBeTypeOf("function");
  });

  it("forward to window.rollDesktop in the desktop app", () => {
    const off = vi.fn();
    const bridge: RollDesktop = {
      version: "0.1.0",
      platform: "linux",
      window: "main",
      setStatus: vi.fn(),
      notify: vi.fn(),
      onDeepLink: vi.fn(() => off),
      openAgents: vi.fn(),
    };
    vi.stubGlobal("window", { rollDesktop: bridge });
    reportToDesktop({ running: 1, pending: [] });
    expect(bridge.setStatus).toHaveBeenCalledWith({ running: 1, pending: [] });
    const listener = () => {};
    expect(onDesktopDeepLink(listener)).toBe(off);
    expect(bridge.onDeepLink).toHaveBeenCalledWith(listener);
  });

  it("open the Agents window only from the desktop app's main window", () => {
    expect(openAgentsWindow({ kind: "new" })).toBe(false);
    const bridge = (window: RollDesktop["window"] | undefined) =>
      ({ version: "0.1.0", platform: "darwin", window, openAgents: vi.fn() }) as unknown as RollDesktop;
    const main = bridge("main");
    vi.stubGlobal("window", { rollDesktop: main });
    expect(openAgentsWindow({ kind: "thread", thread: "thr_1" })).toBe(true);
    expect(main.openAgents).toHaveBeenCalledExactlyOnceWith({ kind: "thread", thread: "thr_1" });
    // In the Agents window the page shows the canvas itself; an older app has no `window`.
    for (const w of ["agents", undefined] as const) {
      const other = bridge(w);
      vi.stubGlobal("window", { rollDesktop: other });
      expect(openAgentsWindow({ kind: "canvas" })).toBe(false);
      expect(other.openAgents).not.toHaveBeenCalled();
    }
  });
});
