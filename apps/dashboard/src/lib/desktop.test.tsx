import { renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { emptyData, type AgentsData } from "./agents-store";
import { useDesktop, type DeepLink, type RollDesktop } from "./desktop";
import { approval, machine, M1, thread } from "@/test/agents";

function dataWith(threads: ReturnType<typeof thread>[], requests: ReturnType<typeof approval>[]): AgentsData {
  const d = emptyData();
  return {
    ...d,
    machines: new Map([[M1, { cursor: 1, machine: M1, value: machine() }]]),
    threads: new Map(threads.map((t, i) => [t.id, { cursor: i + 2, machine: M1, value: t }])),
    requests: new Map(requests.map((r, i) => [r.id, { cursor: i + 10, machine: M1, value: r }])),
  };
}

describe("useDesktop", () => {
  afterEach(() => {
    delete window.rollDesktop;
  });

  it("does nothing in a browser", () => {
    const focus = vi.fn();
    const { unmount } = renderHook(() => useDesktop(dataWith([thread({ status: "running" })], []), focus));
    unmount();
    expect(focus).not.toHaveBeenCalled();
  });

  it("reports the status when it changes and focuses threads from deep links", () => {
    let listener: ((l: DeepLink) => void) | null = null;
    const off = vi.fn();
    const bridge: RollDesktop = {
      version: "0.1.0",
      platform: "darwin",
      window: "agents",
      setStatus: vi.fn(),
      notify: vi.fn(),
      onDeepLink: (l) => {
        listener = l;
        return off;
      },
      openAgents: vi.fn(),
    };
    window.rollDesktop = bridge;
    const focus = vi.fn();
    const t = thread({ status: "waiting" });
    const { rerender, unmount } = renderHook(({ data }) => useDesktop(data, focus), { initialProps: { data: dataWith([t], [approval()]) } });
    expect(bridge.setStatus).toHaveBeenCalledTimes(1);
    const status = vi.mocked(bridge.setStatus).mock.calls[0][0];
    expect(status.pending).toHaveLength(1);
    expect(status.pending[0]).toMatchObject({ thread: t.id, kind: "approval" });
    expect(status.machines).toEqual([{ id: M1, online: machine().online }]);

    // A new snapshot with the same content (a stream frame) sends nothing.
    rerender({ data: dataWith([t], [approval()]) });
    expect(bridge.setStatus).toHaveBeenCalledTimes(1);
    rerender({ data: dataWith([{ ...t, status: "running" }], []) });
    expect(bridge.setStatus).toHaveBeenCalledTimes(2);
    expect(vi.mocked(bridge.setStatus).mock.calls[1][0]).toMatchObject({ running: 1, pending: [] });

    listener!({ kind: "thread", thread: "thr_9" });
    listener!({ kind: "agents" });
    expect(focus).toHaveBeenCalledExactlyOnceWith("t:thr_9");
    unmount();
    expect(off).toHaveBeenCalled();
  });

});
