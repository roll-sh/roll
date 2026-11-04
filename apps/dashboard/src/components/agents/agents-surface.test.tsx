import { act, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { agentsSurface, resetAgentsSurface } from "@/lib/agents-surface";
import { setActiveClient } from "@/lib/agents-client";
import { approval, M1, machine, mirrored, project, run, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents, type AgentsHarness } from "@/test/agents-render";
import { MarkStateProvider } from "../mark-state";
import { AgentsControl } from "./agents-control";
import { CanvasEngine } from "./canvas-engine";
import { defaultProject, NewAgent } from "./new-agent";

let conn: { client: AgentsHarness["client"] | null; services: ReturnType<typeof fakeServices> | null; error: string | null } = { client: null, services: null, error: null };
vi.mock("./use-connection", () => ({ useAgentsConnection: () => conn, useDemoParam: () => false }));

const busy = thread({ status: "running", active_run: "run_1", run_count: 1, activity: "Running `cargo test`" });
const waiting = thread({ id: "thr_2", title: "Upgrade to Next 16.4", status: "waiting", pending_requests: 1, updated_at: 5 });
const entities = [mirrored("project", project(), 1), mirrored("thread", busy, 2), mirrored("thread", waiting, 3), mirrored("run", run(), 4), mirrored("request", approval({ thread: "thr_2" }), 5)];

function setup() {
  const h = agentsHarness({ entities, follow: ["thr_1", "thr_2"] });
  conn = { client: h.client, services: fakeServices(), error: null };
  renderAgents(
    <MarkStateProvider>
      <AgentsControl />
    </MarkStateProvider>,
    h,
  );
  return h;
}

beforeEach(() => {
  resetAgentsSurface();
  window.history.replaceState(null, "", "/dashboard");
});
afterEach(() => {
  setActiveClient(null);
  delete window.rollDesktop;
});

describe("the agents in the header", () => {
  it("counts what runs (a thread waiting on you is waiting, not running) and opens the conversation that waits", async () => {
    setup();
    const pill = screen.getByRole("button", { name: "Agents: 1 running, 1 waiting" });
    expect(pill).toHaveTextContent("1 running");
    await userEvent.click(pill);
    const popup = screen.getByRole("dialog", { name: "Agents" });
    expect(within(popup).getByRole("region", { name: "Thread Upgrade to Next 16.4" })).toBeInTheDocument();
    expect(window.location.hash).toBe("#agents/thr_2");
  });

  it("goes back to the canvas with Escape, and closes with Escape again", async () => {
    setup();
    await userEvent.click(screen.getByRole("button", { name: /Agents: 1 running/ }));
    const popup = screen.getByRole("dialog", { name: "Agents" });
    // The composer has focus and is empty: Escape still leaves.
    await userEvent.keyboard("{Escape}");
    expect(agentsSurface.get().view).toEqual({ kind: "canvas" });
    expect(within(popup).getByLabelText("Agents canvas")).toBeVisible();
    expect(within(popup).getByText("1 waiting for you")).toBeInTheDocument();
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByRole("dialog", { name: "Agents" })).toBeNull();
    expect(window.location.hash).toBe("");
  });

  it("\"New agent\" opens the composer; ⌘J toggles the canvas; #agents links open it", async () => {
    setup();
    await userEvent.click(screen.getByRole("button", { name: "New agent" }));
    expect(screen.getByRole("heading", { name: "What should the agent do?" })).toBeInTheDocument();
    await userEvent.keyboard("{Meta>}j{/Meta}");
    expect(screen.queryByRole("dialog")).toBeNull();
    await userEvent.keyboard("{Meta>}j{/Meta}");
    expect(agentsSurface.get()).toEqual({ open: true, view: { kind: "canvas" } });
    await userEvent.keyboard("{Meta>}j{/Meta}");
    act(() => {
      window.location.hash = "#agents/thr_1";
      window.dispatchEvent(new HashChangeEvent("hashchange"));
    });
    expect(screen.getByRole("region", { name: "Thread Fix the flaky queue test" })).toBeInTheDocument();
  });

  it("in the desktop app's main window, opens the Agents window instead of the popup", async () => {
    const openAgents = vi.fn();
    window.rollDesktop = { version: "0.1.0", platform: "darwin", window: "main", setStatus: vi.fn(), notify: vi.fn(), onDeepLink: () => () => {}, openAgents };
    setup();
    await userEvent.click(screen.getByRole("button", { name: "New agent" }));
    expect(openAgents).toHaveBeenCalledWith({ kind: "new" });
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("signed out, it still shows and says why inside", async () => {
    conn = { client: null, services: null, error: "Sign in to roll code to see your agents." };
    renderAgents(
      <MarkStateProvider>
        <AgentsControl />
      </MarkStateProvider>,
      agentsHarness(),
    );
    await userEvent.click(screen.getByRole("button", { name: "Agents: 0 running, 0 waiting" }));
    expect(screen.getByText("Sign in to roll code to see your agents.")).toBeInTheDocument();
  });
});

describe("a new agent", () => {
  it("starts in the asked-for project, else the last used, else where the newest thread is", () => {
    const a = project({ id: "prj_a" });
    const b = project({ id: "prj_b" });
    const threads = [{ project: "prj_b", updated_at: 9 }];
    expect(defaultProject([a, b], threads, "prj_a", "prj_b")?.id).toBe("prj_a");
    expect(defaultProject([a, b], threads, undefined, "prj_a")?.id).toBe("prj_a");
    expect(defaultProject([a, b], threads)?.id).toBe("prj_b");
    expect(defaultProject([a, b], [], undefined, "prj_gone")?.id).toBe("prj_a");
  });

  it("creates the thread where it was pointed and opens its conversation", async () => {
    const h = agentsHarness({ entities: [mirrored("project", project(), 1)], reply: () => ({ type: "created", id: "thr_new" }) });
    renderAgents(<NewAgent />, h);
    expect(screen.getByRole("combobox", { name: "Machine" })).toHaveValue(M1);
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "Make the tests faster{Enter}");
    expect(h.command.mock.calls.at(-1)?.[2]).toMatchObject({ type: "thread.create", project: "prj_1", prompt: "Make the tests faster" });
    await vi.waitFor(() => expect(h.openThread).toHaveBeenCalledWith("thr_new"));
  });

  it("with no machine yet, offers the ways to connect one", () => {
    renderAgents(<NewAgent />, agentsHarness({ machines: [] }));
    expect(screen.getByRole("heading", { name: "Connect a machine first" })).toBeInTheDocument();
  });
});

describe("the canvas under a trackpad", () => {
  function engine() {
    const container = document.createElement("div");
    const world = document.createElement("div");
    const canvas = document.createElement("canvas");
    container.append(canvas, world);
    document.body.append(container);
    return { e: new CanvasEngine(container, canvas, world), container };
  }
  const wheel = (el: Element, init: WheelEventInit) => el.dispatchEvent(new WheelEvent("wheel", { bubbles: true, cancelable: true, ...init }));

  it("two fingers pan in any direction, a pinch zooms, shift turns a mouse wheel sideways", () => {
    const { e, container } = engine();
    wheel(container, { deltaX: 30, deltaY: 120 });
    expect(e.view).toEqual({ x: -30, y: -120, k: 1 });
    wheel(container, { deltaY: -10, ctrlKey: true });
    expect(e.view.k).toBeGreaterThan(1.1);
    const k = e.view.k;
    wheel(container, { deltaY: 100, shiftKey: true });
    expect(e.view.k).toBe(k);
    expect(e.view.x).toBeLessThan(-30 * k);
    e.destroy();
  });

  it("Safari's pinch gestures zoom too, and scrollable content keeps its wheel", () => {
    const { e, container } = engine();
    container.dispatchEvent(Object.assign(new Event("gesturestart", { cancelable: true }), { scale: 1 }));
    container.dispatchEvent(Object.assign(new Event("gesturechange", { cancelable: true }), { scale: 2 }));
    expect(e.view.k).toBeCloseTo(2);
    container.dispatchEvent(new Event("gestureend"));
    const inner = document.createElement("div");
    inner.dataset.scroll = "";
    container.append(inner);
    const before = { ...e.view };
    wheel(inner, { deltaY: 50 });
    expect(e.view).toEqual(before);
    e.destroy();
  });
});
