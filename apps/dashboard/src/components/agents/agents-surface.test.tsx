import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { agentsSurface, resetAgentsSurface, setAgentsView } from "@/lib/agents-surface";
import { setActiveClient } from "@/lib/agents-client";
import { approval, machine, mirrored, project, run, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents, type AgentsHarness } from "@/test/agents-render";
import { MarkStateProvider } from "../mark-state";
import { AgentsControl } from "./agents-control";
import { AgentsPage } from "./agents-page";
import { CanvasEngine } from "./canvas-engine";
import { Launcher } from "./new-agent";
import { defaultProject } from "./new-thread";

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

  it("with no machines yet, shows the first-run screen and doesn't call nothing \"All idle\"", async () => {
    const h = agentsHarness({ machines: [] });
    conn = { client: h.client, services: fakeServices(), error: null };
    renderAgents(
      <MarkStateProvider>
        <AgentsControl />
      </MarkStateProvider>,
      h,
    );
    await userEvent.click(screen.getByRole("button", { name: /^Agents:/ }));
    // Connected, with nothing (the harness's relay never says its stream is open).
    act(() => h.client.show(h.client.data));
    const popup = screen.getByRole("dialog", { name: "Agents" });
    const start = within(popup).getByRole("region", { name: "Get started" });
    expect(start).toHaveTextContent("Connect your computer");
    expect(start).toHaveTextContent("Rent a sandbox");
    expect(popup).not.toHaveTextContent("All idle");
    expect(popup).toHaveTextContent("No machines yet");
  });

  it("goes back to the canvas with Escape, and closes with Escape again", async () => {
    setup();
    await userEvent.click(screen.getByRole("button", { name: /Agents: 1 running/ }));
    const popup = screen.getByRole("dialog", { name: "Agents" });
    // The composer has focus and is empty: Escape still leaves.
    await userEvent.keyboard("{Escape}");
    expect(agentsSurface.get().view).toEqual({ kind: "canvas" });
    expect(within(popup).getByLabelText("Agents canvas")).toBeVisible();
    expect(within(popup).getByRole("button", { name: "1 waiting for you" })).toBeInTheDocument();
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByRole("dialog", { name: "Agents" })).toBeNull();
    expect(window.location.hash).toBe("");
  });

  it("Escape closes what's open inside a conversation first (the diff, the publish form), then the conversation", async () => {
    setup();
    await userEvent.click(screen.getByRole("button", { name: /Agents: 1 running/ }));
    const panel = screen.getByRole("complementary", { name: "Conversation" });
    await userEvent.click(within(panel).getByRole("button", { name: "Diff" }));
    expect(within(panel).getByRole("complementary", { name: "Diff" })).toBeInTheDocument();
    await userEvent.keyboard("{Escape}");
    expect(within(panel).queryByRole("complementary", { name: "Diff" })).toBeNull();
    expect(agentsSurface.get().view).toMatchObject({ kind: "thread" });
    await userEvent.keyboard("{Escape}");
    expect(agentsSurface.get().view).toEqual({ kind: "canvas" });
  });

  it("gives focus back when something closes: the conversation's card, and what had it before ⌘K", async () => {
    setup();
    await userEvent.click(screen.getByRole("button", { name: /Agents: 1 running/ }));
    const thread = (agentsSurface.get().view as { thread: string }).thread;
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(document.activeElement).toBe(document.querySelector(`[data-card][data-key="t:${thread}"]`)));
    const display = screen.getByRole("button", { name: /Display/ });
    display.focus();
    await userEvent.keyboard("{Control>}k{/Control}");
    expect(await screen.findByRole("dialog", { name: /command|palette|search/i })).toBeInTheDocument();
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(display).toHaveFocus());
  });

  it("opens a conversation whose record comes after the reply (a fork), and leaves one that's deleted", async () => {
    const h = setup();
    await userEvent.click(screen.getByRole("button", { name: /Agents: 1 running/ }));
    await act(() => new Promise((r) => setTimeout(r, 0)));
    act(() => h.client.show(h.client.data)); // live
    expect(h.client.getSnapshot().conn).toBe("live");
    // A fork's reply names it before its record is on the stream: it isn't "gone" yet.
    act(() => setAgentsView({ kind: "thread", thread: "thr_fork" }));
    await act(() => new Promise((r) => setTimeout(r, 0)));
    act(() => h.client.show(h.client.data)); // the stream is up (following the thread reopened it)
    expect(h.client.getSnapshot().conn).toBe("live");
    await act(() => new Promise((r) => setTimeout(r, 50)));
    expect(agentsSurface.get().view).toEqual({ kind: "thread", thread: "thr_fork" });
    act(() => h.client.enqueue({ type: "entity", ...mirrored("thread", thread({ id: "thr_fork", title: "Fork of the flaky test" }), 9) }));
    expect(await screen.findByRole("complementary", { name: "Conversation" })).toHaveTextContent("Fork of the flaky test");
    // Deleted (here a moment ago): back to the canvas at once.
    act(() => h.client.enqueue({ type: "entity", cursor: 10, machine: "mch_1", entity: { kind: "removed", entity: "thread", id: "thr_fork", thread: null } }));
    await act(() => new Promise((r) => setTimeout(r, 0)));
    act(() => h.client.show(h.client.data)); // the stream is up again (following the thread reopened it)
    expect(agentsSurface.get().view).toEqual({ kind: "canvas" });
  });

  it("\"New agent\" opens the launcher; ⌘J toggles the canvas; #agents links open it", async () => {
    setup();
    await userEvent.click(screen.getByRole("button", { name: "New agent" }));
    expect(screen.getByRole("dialog", { name: "New agent" })).toBeInTheDocument();
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

describe("the canvas order across devices", () => {
  it("a card moved here is saved to roll; another device's save loads here", async () => {
    localStorage.clear();
    const h = setup();
    const services = conn.services!;
    await userEvent.keyboard("{Meta>}j{/Meta}");
    await vi.waitFor(() => expect(services.canvas.load).toHaveBeenCalledWith("acme"));
    const card = screen.getByRole("button", { name: "Thread Upgrade to Next 16.4" });
    // The canvas document ({ order, views }), kept here and by roll.
    const kids = () => JSON.parse(localStorage.getItem("roll:agents:canvas:acme") ?? "{}").order?.["p:prj_1"];
    card.focus();
    // It's the second of the project's two threads: up makes it the first.
    await userEvent.keyboard("{Alt>}{ArrowUp}{/Alt}");
    const mine = kids();
    expect(mine).toEqual(["t:thr_2", "t:thr_1"]);
    await vi.waitFor(() => expect(services.canvas.save).toHaveBeenLastCalledWith("acme", expect.objectContaining({ order: { "p:prj_1": mine } })));
    // Another device puts them the other way round; its save comes in on the stream.
    const { revision } = await services.canvas.save("acme", { order: { "p:prj_1": [...mine].reverse() } });
    act(() => h.client.enqueue({ type: "canvas", cursor: 50, revision, updated_at: 1 }));
    await vi.waitFor(() => expect(kids()).toEqual([...mine].reverse()));
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
    // The daemon's reply comes first, then the thread's record (it opens once that's in).
    const h: AgentsHarness = agentsHarness({
      entities: [mirrored("project", project(), 1)],
      reply: () => {
        setTimeout(() => h.client.enqueue({ type: "entity", ...mirrored("thread", thread({ id: "thr_new" }), 2) }), 0);
        return { type: "created", id: "thr_new" };
      },
    });
    renderAgents(<Launcher onClose={vi.fn()} />, h);
    expect(screen.getByRole("combobox", { name: "Machine" })).toHaveTextContent("ada-mbp");
    expect(screen.getByRole("combobox", { name: "Project" })).toHaveTextContent("roll");
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "Make the tests faster{Enter}");
    expect(h.command.mock.calls.at(-1)?.[2]).toMatchObject({ type: "thread.create", project: "prj_1", prompt: "Make the tests faster" });
    await vi.waitFor(() => expect(h.openThread).toHaveBeenCalledWith("thr_new"));
  });

  it("with no machine yet, offers the ways to connect one", () => {
    renderAgents(<Launcher onClose={vi.fn()} />, agentsHarness({ machines: [] }));
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

describe("the agents page (/dashboard/agents)", () => {
  it("⌘, opens the owner's settings there too (the header control isn't on that page)", async () => {
    const h = agentsHarness({ entities, follow: ["thr_1", "thr_2"] });
    conn = { client: h.client, services: fakeServices(), error: null };
    window.history.replaceState(null, "", "/dashboard/agents");
    renderAgents(
      <MarkStateProvider>
        <AgentsPage />
      </MarkStateProvider>,
      h,
    );
    await screen.findByLabelText("Agents canvas");
    await userEvent.keyboard("{Control>},{/Control}");
    expect(agentsSurface.get().view).toEqual({ kind: "settings", target: "owner" });
  });

  it("follows #agents links as the popup does: where it opens, and one followed later", async () => {
    const h = agentsHarness({ entities, follow: ["thr_1", "thr_2"] });
    conn = { client: h.client, services: fakeServices(), error: null };
    window.history.replaceState(null, "", "/dashboard/agents#agents/thr_2");
    renderAgents(
      <MarkStateProvider>
        <AgentsPage />
      </MarkStateProvider>,
      h,
    );
    expect(await screen.findByRole("region", { name: "Thread Upgrade to Next 16.4" })).toBeInTheDocument();
    act(() => {
      window.location.hash = "#agents/settings/owner";
      window.dispatchEvent(new HashChangeEvent("hashchange"));
    });
    expect(agentsSurface.get().view).toEqual({ kind: "settings", target: "owner" });
  });
});
