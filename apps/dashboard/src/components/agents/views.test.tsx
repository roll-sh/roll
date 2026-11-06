import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { setActiveClient } from "@/lib/agents-client";
import { agentsSurface, resetAgentsSurface } from "@/lib/agents-surface";
import { loadCurrentView, loadDoc } from "@/lib/agents-views";
import { approval, mirrored, node, project, run, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";
import { AgentsSurface } from "./agents-page";

const busy = thread({ status: "running", active_run: "run_1", run_count: 1, activity: "Running `cargo test`", updated_at: Date.now() });
const waiting = thread({ id: "thr_2", title: "Upgrade to Next 16.4", status: "waiting", pending_requests: 1, active_run: "run_2", run_count: 1, updated_at: Date.now() });
const entities = [
  mirrored("project", project(), 1),
  mirrored("thread", busy, 2),
  mirrored("thread", waiting, 3),
  mirrored("run", run(), 4),
  mirrored("run", run({ id: "run_2", thread: "thr_2", status: "waiting", prompt: "Upgrade Next" }), 5),
  mirrored("node", node({ type: "request", request: "req_1" }, { id: "nod_9", thread: "thr_2", run: "run_2" }), 6),
  mirrored("request", approval({ thread: "thr_2", run: "run_2" }), 7),
];

function setup(services = fakeServices()) {
  const h = agentsHarness({ entities, follow: ["thr_1", "thr_2"] });
  renderAgents(<AgentsSurface client={h.client} services={services} demo={false} chrome="popup" />, h);
  return { h, services };
}

const tab = (name: RegExp | string) => screen.getByRole("tab", { name });
const card = (title: string) => screen.queryByRole("button", { name: `Thread ${title}` });
/** What roll was last asked to keep. */
const lastSaved = (services: ReturnType<typeof fakeServices>) => (services.canvas.save as unknown as { mock: { calls: [string, Record<string, unknown>][] } }).mock.calls.at(-1)?.[1];

beforeEach(() => {
  localStorage.clear();
  resetAgentsSurface();
});
afterEach(() => setActiveClient(null));

describe("canvas views", () => {
  it("starts with All and the built-in views, each counting what it picks, and filters the canvas", async () => {
    setup();
    const bar = screen.getByRole("tablist", { name: "Canvas views" });
    expect(within(bar).getAllByRole("tab").map((t) => t.textContent)).toEqual(["All", "Waiting on me1", "Running1"]);
    expect(tab("All")).toHaveAttribute("aria-selected", "true");
    expect(card("Fix the flaky queue test")).toBeInTheDocument();
    expect(card("Upgrade to Next 16.4")).toBeInTheDocument();

    await userEvent.click(tab(/Waiting on me/));
    expect(tab(/Waiting on me/)).toHaveAttribute("aria-selected", "true");
    expect(card("Upgrade to Next 16.4")).toBeInTheDocument();
    expect(card("Fix the flaky queue test")).toBeNull();
    // Which view shows is this device's.
    expect(loadCurrentView("acme")).toBe("v_waiting");

    // [ and ] step through the views; ⌥1 is All.
    await userEvent.keyboard("]");
    expect(tab(/Running/)).toHaveAttribute("aria-selected", "true");
    expect(card("Upgrade to Next 16.4")).toBeNull();
    await userEvent.keyboard("{Alt>}1{/Alt}");
    expect(tab("All")).toHaveAttribute("aria-selected", "true");
  });

  it("saves All's filter as a view, synced to roll", async () => {
    const { services } = setup();
    // Pasted, not typed key by key: every key re-renders the whole surface.
    await userEvent.click(screen.getByRole("textbox", { name: "Filter threads" }));
    await userEvent.paste("next");
    expect(card("Fix the flaky queue test")).toBeNull();
    expect(card("Upgrade to Next 16.4")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Save view" }));
    const dialog = screen.getByRole("dialog", { name: "New view" });
    await userEvent.click(within(dialog).getByRole("textbox", { name: "View name" }));
    await userEvent.paste("Next work");
    await userEvent.keyboard("{Enter}");
    expect(tab(/Next work/)).toHaveAttribute("aria-selected", "true");
    // All's filter went into the view.
    await userEvent.click(tab("All"));
    expect(screen.getByRole("textbox", { name: "Filter threads" })).toHaveValue("");
    await waitFor(() => expect(lastSaved(services)).toMatchObject({ views: [{ id: "v_waiting" }, { id: "v_running" }, { name: "Next work", filter: { text: "next" } }] }));
  });
});

describe("kept by roll and on this device", () => {
  it("loads an old { order } document with the built-in views, and another device's save", async () => {
    const services = fakeServices();
    await services.canvas.save("acme", { order: { "p:prj_1": ["t:thr_2", "t:thr_1"] } });
    const { h } = setup(services);
    await waitFor(() => expect(screen.getAllByRole("tab")).toHaveLength(3));
    expect(loadDoc("acme").order).toEqual({ "p:prj_1": ["t:thr_2", "t:thr_1"] });
    // Another device adds a view: the stream says the canvas changed, and it loads.
    const { revision } = await services.canvas.save("acme", { order: {}, views: [{ id: "v_x", name: "Elsewhere", filter: { status: ["error"] } }] });
    act(() => h.client.enqueue({ type: "canvas", cursor: 99, revision, updated_at: 1 }));
    await waitFor(() => expect(screen.getAllByRole("tab").map((t) => t.textContent)).toEqual(["All", "Elsewhere"]));
  });
});

describe("moving around the canvas from the keyboard", () => {
  const web = [mirrored("project", project({ id: "prj_2", name: "web", path: "/Users/ada/web" }), 8), mirrored("thread", thread({ id: "thr_3", project: "prj_2", title: "Dark mode" }), 9)];
  const cardKey = () => (document.activeElement as HTMLElement | null)?.closest<HTMLElement>("[data-card]")?.dataset.key;
  function start() {
    const h = agentsHarness({ entities: [...entities, ...web], follow: ["thr_1", "thr_2"] });
    renderAgents(<AgentsSurface client={h.client} services={fakeServices()} demo={false} chrome="popup" />, h);
  }

  it("arrows go from card to card along the tree, Enter opens, Space goes to what's waiting", async () => {
    start();
    card("Fix the flaky queue test")!.focus();
    await userEvent.keyboard("{ArrowLeft}");
    expect(cardKey()).toBe("p:prj_1");
    await userEvent.keyboard("{ArrowLeft}");
    expect(cardKey()).toMatch(/^m:/);
    await userEvent.keyboard("{ArrowRight}");
    expect(cardKey()).toBe("p:prj_1");
    await userEvent.keyboard("{ArrowDown}");
    expect(cardKey()).toBe("p:prj_2");
    // Enter on a project opens its settings beside the canvas.
    await userEvent.keyboard("{Enter}");
    expect(agentsSurface.get().view).toEqual({ kind: "settings", target: "p:prj_2" });
    resetAgentsSurface();
    // Space on a card goes to the request waiting (its answer focused), wherever it is.
    card("Dark mode")!.focus();
    await userEvent.keyboard(" ");
    expect(cardKey()).toBe("r:req_1");
    expect(document.activeElement).toHaveTextContent("Approve");
    // Nothing opened on the way.
    expect(agentsSurface.get().view).toEqual({ kind: "canvas" });
  });
});

describe("the display menu", () => {
  it("sets density and what's shown for the view, saved with it", async () => {
    const services = fakeServices();
    const h = agentsHarness({ entities: [...entities, mirrored("thread", thread({ id: "thr_q", title: "Docs pass" }), 8)], follow: ["thr_1", "thr_2"] });
    renderAgents(<AgentsSurface client={h.client} services={services} demo={false} chrome="popup" />, h);
    const canvas = screen.getByLabelText("Agents canvas");
    expect(canvas).toHaveAttribute("data-density", "comfortable");
    await userEvent.click(screen.getByRole("button", { name: "Display" }));
    const menu = await screen.findByRole("dialog", { name: "Display options" });

    await userEvent.click(within(menu).getByRole("button", { name: "Compact" }));
    expect(canvas).toHaveAttribute("data-density", "compact");

    expect(card("Docs pass")).toBeInTheDocument();
    await userEvent.click(within(menu).getByRole("switch", { name: "Idle threads" }));
    expect(card("Docs pass")).toBeNull();
    // What's working or waiting stays.
    expect(card("Fix the flaky queue test")).toBeInTheDocument();
    expect(card("Upgrade to Next 16.4")).toBeInTheDocument();
    await waitFor(() => expect(lastSaved(services)).toMatchObject({ display: { density: "compact", hide: ["idle"] } }));
  });
});
