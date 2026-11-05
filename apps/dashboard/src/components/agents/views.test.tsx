import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { setActiveClient } from "@/lib/agents-client";
import { agentsSurface, resetAgentsSurface } from "@/lib/agents-surface";
import { resetShelves } from "@/lib/agents-shelf";
import { ALL, loadCurrentView, loadDoc, saveCurrentView, saveDoc, addView, parseDoc } from "@/lib/agents-views";
import { approval, mirrored, node, project, run, terminal, thread } from "@/test/agents";
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

  it("saves All's filter as a view, renames it, moves it and deletes it, synced to roll", async () => {
    const { services } = setup();
    await userEvent.type(screen.getByRole("textbox", { name: "Filter threads" }), "next");
    expect(card("Fix the flaky queue test")).toBeNull();
    expect(card("Upgrade to Next 16.4")).toBeInTheDocument();
    // ⌥S once nothing is being typed into.
    screen.getByRole("textbox", { name: "Filter threads" }).blur();
    await userEvent.keyboard("{Alt>}s{/Alt}");
    const dialog = screen.getByRole("dialog", { name: "New view" });
    await userEvent.type(within(dialog).getByRole("textbox", { name: "View name" }), "Next work{Enter}");
    expect(tab(/Next work/)).toHaveAttribute("aria-selected", "true");
    // All's filter went into the view.
    await userEvent.click(tab("All"));
    expect(screen.getByRole("textbox", { name: "Filter threads" })).toHaveValue("");
    await waitFor(() => expect(lastSaved(services)).toMatchObject({ views: [{ id: "v_waiting" }, { id: "v_running" }, { name: "Next work", filter: { text: "next" } }] }));

    await userEvent.click(tab(/Next work/));
    await userEvent.click(screen.getByRole("button", { name: "Next work options" }));
    // The menu floats outside the views bar (which scrolls sideways and clipped it), and the
    // keyboard moves through it.
    const menu = await screen.findByRole("menu");
    expect(screen.getByRole("tablist", { name: "Canvas views" }).contains(menu)).toBe(false);
    await userEvent.keyboard("{ArrowDown}");
    expect(within(menu).getAllByRole("menuitem").some((i) => i === document.activeElement)).toBe(true);
    await userEvent.click(await screen.findByRole("menuitem", { name: "Rename" }));
    const name = screen.getByRole("textbox", { name: "View name" });
    await userEvent.clear(name);
    await userEvent.type(name, "Frontend{Enter}");
    await userEvent.click(screen.getByRole("button", { name: "Frontend options" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Move left" }));
    expect(screen.getAllByRole("tab").map((t) => t.textContent)).toEqual(["All", "Waiting on me1", "Frontend1", "Running1"]);
    await userEvent.click(screen.getByRole("button", { name: "Frontend options" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Delete view" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Delete “Frontend”?" }));
    expect(screen.queryByRole("tab", { name: /Frontend/ })).toBeNull();
    expect(tab("All")).toHaveAttribute("aria-selected", "true");
    await waitFor(() => expect((lastSaved(services)!.views as unknown[]).length).toBe(2));
  });

  it("edits a view's filters as toggles", async () => {
    setup();
    await userEvent.click(tab(/Running/));
    await userEvent.click(screen.getByRole("button", { name: "Running filters" }));
    const dialog = screen.getByRole("dialog", { name: "View filters" });
    const status = within(dialog).getByRole("group", { name: "Status" });
    expect(within(status).getByRole("button", { name: "Running" })).toHaveAttribute("aria-pressed", "true");
    await userEvent.click(within(status).getByRole("button", { name: "Waiting on me" }));
    expect(card("Upgrade to Next 16.4")).toBeInTheDocument();
    expect(card("Fix the flaky queue test")).toBeInTheDocument();
    await userEvent.click(within(status).getByRole("button", { name: "Running" }));
    expect(card("Fix the flaky queue test")).toBeNull();
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByRole("dialog", { name: "View filters" })).toBeNull();
    // Still on the canvas (Escape closed only the editor).
    expect(screen.getByLabelText("Agents canvas")).toBeInTheDocument();
  });

  it("says so calmly when a view picks nothing", async () => {
    const h = agentsHarness({ entities: [mirrored("project", project(), 1), mirrored("thread", busy, 2)] });
    renderAgents(<AgentsSurface client={h.client} services={fakeServices()} demo={false} chrome="popup" />, h);
    await userEvent.click(tab(/Waiting on me/));
    expect(screen.getByText("Nothing is waiting on you")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Show all" }));
    expect(tab("All")).toHaveAttribute("aria-selected", "true");
  });
});

describe("pinned cards", () => {
  it("pins a conversation onto the view: live, answerable in place, with its composer; unpins", async () => {
    const { services } = setup();
    await userEvent.click(tab(/Waiting on me/));
    await userEvent.click(screen.getByRole("button", { name: "Pin the conversation on Waiting on me" }));
    const pinned = screen.getByRole("region", { name: "Pinned conversation Upgrade to Next 16.4" });
    // The request is answered inside the card, not beside the thread.
    expect(within(pinned).getByRole("button", { name: "Approve" })).toBeInTheDocument();
    expect(screen.queryByLabelText("Waiting for you")).toBeNull();
    expect(within(pinned).getByRole("textbox")).toBeInTheDocument();
    await waitFor(() => expect(lastSaved(services)).toMatchObject({ views: [{ id: "v_waiting", pins: [{ kind: "conversation", id: "thr_2", w: 520, h: 600 }] }, { id: "v_running" }] }));
    // Only on that view.
    await userEvent.click(tab("All"));
    expect(screen.queryByRole("region", { name: /Pinned conversation/ })).toBeNull();
    await userEvent.click(tab(/Waiting on me/));
    await userEvent.click(within(screen.getByRole("region", { name: "Pinned conversation Upgrade to Next 16.4" })).getByRole("button", { name: "Unpin from this view" }));
    expect(screen.queryByRole("region", { name: /Pinned conversation/ })).toBeNull();
  });

  it("resizes from the corner (and the keyboard), remembered per view", async () => {
    const { services } = setup();
    await userEvent.click(within(screen.getByRole("button", { name: "Thread Upgrade to Next 16.4" }).parentElement!).getByRole("button", { name: "Pin the diff" }));
    const pinned = screen.getByRole("region", { name: /Pinned diff/ });
    expect(pinned).toHaveStyle({ width: "680px", height: "520px" });
    const corner = within(pinned).getByRole("separator", { name: "Resize" });
    corner.focus();
    await userEvent.keyboard("{Shift>}{ArrowRight}{ArrowDown}{/Shift}");
    expect(pinned).toHaveStyle({ width: "760px", height: "600px" });
    await waitFor(() => expect(lastSaved(services)).toMatchObject({ pins: [{ kind: "diff", w: 760, h: 600 }] }));
  });

  it("a terminal card's tools float over it as one toolbar: pin, and close (which hangs it up)", async () => {
    const h = agentsHarness({ entities: [...entities, mirrored("terminal", terminal({ id: "trm_1", title: "claude /login" }), 8)], follow: ["thr_1", "thr_2"] });
    renderAgents(<AgentsSurface client={h.client} services={fakeServices()} demo={false} chrome="popup" />, h);
    const slot = screen.getByLabelText("Terminal claude /login").parentElement!;
    const tools = within(slot).getByRole("button", { name: "Pin the terminal" }).parentElement!;
    await userEvent.click(within(tools).getByRole("button", { name: "Close terminal" }));
    expect(h.command).toHaveBeenCalledWith("mch_1", expect.any(String), { type: "terminal.close", terminal: "trm_1" });
    // Nothing else in the card closes it, so nothing hides under the toolbar.
    expect(within(screen.getByLabelText("Terminal claude /login")).queryByRole("button", { name: "Close terminal" })).toBeNull();
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

  it("keeps the document and the order from before views on this device; the current view is per device", () => {
    localStorage.setItem("roll:agents:order:o", JSON.stringify({ "m:mch_1": ["p:b", "p:a"] }));
    localStorage.setItem("roll:agents:positions:o", "{}");
    const doc = loadDoc("o");
    expect(doc.order).toEqual({ "m:mch_1": ["p:b", "p:a"] });
    expect(localStorage.getItem("roll:agents:positions:o")).toBeNull();
    saveDoc("o", addView(doc, "Mine", {}, "v_m"));
    expect(localStorage.getItem("roll:agents:order:o")).toBeNull();
    expect(loadDoc("o").views.map((v) => v.id)).toEqual(["v_waiting", "v_running", "v_m"]);
    expect(loadCurrentView("o")).toBe(ALL);
    saveCurrentView("o", "v_m");
    expect(loadCurrentView("o")).toBe("v_m");
    expect(parseDoc(JSON.parse(localStorage.getItem("roll:agents:canvas:o")!))).toEqual(loadDoc("o"));
  });
});

describe("the canvas's direction", () => {
  const canvas = () => screen.getByLabelText("Agents canvas");
  const asPhone = () => {
    const mm = window.matchMedia;
    vi.spyOn(window, "matchMedia").mockImplementation((q: string) => ({ ...mm(q), matches: q.includes("max-width: 640px") }));
  };
  afterEach(() => vi.restoreAllMocks());

  it("a phone's waiting request folds to one line, and the zoom bar moves to the top while it shows", async () => {
    asPhone();
    setup();
    // Plain selectors: the canvas is a big tree for role queries.
    const $ = (sel: string) => document.querySelector<HTMLElement>(sel);
    await waitFor(() => expect($('[data-phone-waiting]')).not.toBeNull());
    expect($("[data-waiting]")?.contains(canvas())).toBe(true);
    expect($('[aria-label="Zoom out (−)"]')?.parentElement?.className).toContain("[[data-waiting]_&]:top-3");
    await userEvent.click($('[data-phone-waiting] [aria-label="Fold"]')!);
    expect($('[data-phone-waiting]')).toBeNull();
    await userEvent.click($('[aria-label="1 waiting for you: show"]')!);
    expect($('[data-phone-waiting]')).not.toBeNull();
  });

  it("grows right on a wide screen; the toolbar turns a view top-down, saved with that view in roll", async () => {
    const { services } = setup();
    expect(canvas()).toHaveAttribute("data-direction", "horizontal");
    await userEvent.click(screen.getByRole("button", { name: "Top-down (⇧D)" }));
    expect(canvas()).toHaveAttribute("data-direction", "vertical");
    await waitFor(() => expect(lastSaved(services)).toMatchObject({ display: { direction: "vertical" } }));
    // Each view has its own.
    await userEvent.click(tab(/Running/));
    expect(canvas()).toHaveAttribute("data-direction", "horizontal");
    await userEvent.keyboard("{Shift>}D{/Shift}");
    expect(canvas()).toHaveAttribute("data-direction", "vertical");
    await waitFor(() => expect(lastSaved(services)).toMatchObject({ views: [{ id: "v_waiting" }, { id: "v_running", display: { direction: "vertical" } }] }));
    await userEvent.click(tab("All"));
    expect(canvas()).toHaveAttribute("data-direction", "vertical");
  });

  it("from ⌘K too", async () => {
    setup();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(screen.getByRole("combobox"), "top-down");
    await userEvent.keyboard("{Enter}");
    expect(canvas()).toHaveAttribute("data-direction", "vertical");
    await waitFor(() => expect(screen.queryByRole("combobox")).toBeNull());
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(await screen.findByRole("combobox"), "left to right");
    await userEvent.keyboard("{Enter}");
    expect(canvas()).toHaveAttribute("data-direction", "horizontal");
  });

  it("a phone reads top-down by default, and its choice is its own", async () => {
    asPhone();
    const services = fakeServices();
    // The laptop chose left to right; the phone still starts top-down.
    await services.canvas.save("acme", { order: {}, display: { direction: "horizontal" } });
    setup(services);
    await waitFor(() => expect(canvas()).toHaveAttribute("data-direction", "vertical"));
    await userEvent.click(screen.getByRole("button", { name: "Top-down (⇧D)" }));
    expect(canvas()).toHaveAttribute("data-direction", "horizontal");
    await waitFor(() => expect(lastSaved(services)).toMatchObject({ display: { direction: "horizontal", phoneDirection: "horizontal" } }));
  });
});

describe("moving around the canvas from the keyboard", () => {
  const web = [mirrored("project", project({ id: "prj_2", name: "web", path: "/Users/ada/web" }), 8), mirrored("thread", thread({ id: "thr_3", project: "prj_2", title: "Dark mode" }), 9)];
  const cardKey = () => (document.activeElement as HTMLElement | null)?.closest<HTMLElement>("[data-card]")?.dataset.key;
  function start() {
    const h = agentsHarness({ entities: [...entities, ...web], follow: ["thr_1", "thr_2"] });
    renderAgents(<AgentsSurface client={h.client} services={fakeServices()} demo={false} chrome="popup" />, h);
  }

  it("⇧S opens the shelf (Escape closes it)", async () => {
    start();
    await userEvent.keyboard("{Shift>}S{/Shift}");
    const shelf = await screen.findByRole("dialog", { name: "Shelf" });
    expect(within(shelf).getByRole("combobox", { name: "Search the shelf" })).toHaveFocus();
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Shelf" })).not.toBeInTheDocument());
    resetShelves();
  });

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

  it("F fills the canvas with a card's project; the chip or Escape shows everything again", async () => {
    start();
    card("Fix the flaky queue test")!.focus();
    await userEvent.keyboard("f");
    expect(card("Dark mode")).toBeNull();
    expect(card("Upgrade to Next 16.4")).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("roll");
    await userEvent.keyboard("{Escape}");
    expect(card("Dark mode")).toBeInTheDocument();
    // Still the canvas: Escape let go of the project first.
    expect(screen.getByLabelText("Agents canvas")).toBeInTheDocument();
    card("Dark mode")!.focus();
    await userEvent.keyboard("f");
    expect(card("Fix the flaky queue test")).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Stop focusing web" }));
    expect(card("Fix the flaky queue test")).toBeInTheDocument();
  });

  it("from the project's menu and ⌘K too", async () => {
    start();
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(screen.getByRole("combobox"), "focus a project");
    await userEvent.keyboard("{Enter}");
    await userEvent.type(screen.getByRole("combobox"), "web");
    await userEvent.keyboard("{Enter}");
    expect(card("Fix the flaky queue test")).toBeNull();
    expect(card("Dark mode")).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByRole("combobox")).toBeNull());
    await userEvent.keyboard("{Meta>}k{/Meta}");
    await userEvent.type(await screen.findByRole("combobox"), "show every project");
    await userEvent.keyboard("{Enter}");
    expect(card("Fix the flaky queue test")).toBeInTheDocument();
    const roll = screen.getByLabelText("Project roll").closest<HTMLElement>("[data-card]")!;
    await userEvent.click(within(roll).getByRole("button", { name: "Project" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Focus on this project only" }));
    expect(card("Dark mode")).toBeNull();
  });
});

describe("the display menu", () => {
  it("sets density, grouping and what's shown for the view, saved with it", async () => {
    const services = fakeServices();
    const extra = [mirrored("thread", thread({ id: "thr_q", title: "Docs pass" }), 8), mirrored("terminal", terminal(), 9)];
    const h = agentsHarness({ entities: [...entities, ...extra], follow: ["thr_1", "thr_2"] });
    renderAgents(<AgentsSurface client={h.client} services={services} demo={false} chrome="popup" />, h);
    const canvas = screen.getByLabelText("Agents canvas");
    expect(canvas).toHaveAttribute("data-density", "comfortable");
    await userEvent.click(screen.getByRole("button", { name: "Display" }));
    const menu = await screen.findByRole("dialog", { name: "Display options" });

    await userEvent.click(within(menu).getByRole("button", { name: "Compact" }));
    expect(canvas).toHaveAttribute("data-density", "compact");

    expect(screen.getByLabelText("Machine ada-mbp")).toBeInTheDocument();
    await userEvent.click(within(menu).getByRole("button", { name: "Project" }));
    expect(screen.queryByLabelText("Machine ada-mbp")).toBeNull();
    expect(screen.getByLabelText("Project roll")).toBeInTheDocument();

    expect(screen.getByLabelText("Terminal zsh")).toBeInTheDocument();
    await userEvent.click(within(menu).getByRole("switch", { name: "Terminals" }));
    expect(screen.queryByLabelText("Terminal zsh")).toBeNull();

    expect(card("Docs pass")).toBeInTheDocument();
    await userEvent.click(within(menu).getByRole("switch", { name: "Idle threads" }));
    expect(card("Docs pass")).toBeNull();
    // What's working or waiting stays.
    expect(card("Fix the flaky queue test")).toBeInTheDocument();
    expect(card("Upgrade to Next 16.4")).toBeInTheDocument();
    await waitFor(() => expect(lastSaved(services)).toMatchObject({ display: { density: "compact", group: "project", hide: ["idle", "terminals"] } }));

    // Another view keeps its own.
    await userEvent.keyboard("{Escape}");
    await userEvent.click(tab(/Running/));
    expect(canvas).toHaveAttribute("data-density", "comfortable");
    await userEvent.click(tab("All"));
    await userEvent.click(screen.getByRole("button", { name: "Display" }));
    await userEvent.click(await screen.findByRole("button", { name: "Reset to default" }));
    expect(canvas).toHaveAttribute("data-density", "comfortable");
    expect(card("Docs pass")).toBeInTheDocument();
  }, 20_000);
});
