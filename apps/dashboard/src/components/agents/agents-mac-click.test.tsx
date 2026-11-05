/**
 * The agents surface under a macOS click (Safari, Firefox on a Mac): a button, link or menu row
 * never takes focus, and what had it blurs with no `relatedTarget`. Chrome and Linux WebKit focus
 * the button, so these paths only break there; `macClick` (src/test/mac-click.ts) makes them here.
 */
import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { setActiveClient } from "@/lib/agents-client";
import { agentsSurface, resetAgentsSurface, setAgentsView } from "@/lib/agents-surface";
import { approval, machine, mirrored, node, project, question, run, terminal, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";
import { macClick } from "@/test/mac-click";
import { AgentsSurface } from "./agents-page";
import { Launcher } from "./new-agent";
import { RequestCard } from "./request-card";
import { Conversation } from "./session";

beforeEach(() => {
  localStorage.clear();
  resetAgentsSurface();
});
afterEach(() => setActiveClient(null));

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
  mirrored("terminal", terminal(), 8),
];

describe("a macOS click", () => {
  it("works the display popover: each choice applies and it stays open", async () => {
    const h = agentsHarness({ entities, follow: ["thr_1", "thr_2"] });
    renderAgents(<AgentsSurface client={h.client} services={fakeServices()} demo={false} chrome="popup" />, h);
    const canvas = screen.getByLabelText("Agents canvas");
    macClick(screen.getByRole("button", { name: "Display" }));
    const menu = await screen.findByRole("dialog", { name: "Display options" });
    macClick(within(menu).getByRole("button", { name: "Compact" }));
    await waitFor(() => expect(canvas).toHaveAttribute("data-density", "compact"));
    expect(screen.getByRole("dialog", { name: "Display options" })).toBeInTheDocument();
    macClick(within(menu).getByRole("switch", { name: "Terminals" }));
    await waitFor(() => expect(screen.queryByLabelText("Terminal zsh")).toBeNull());
    expect(screen.getByRole("dialog", { name: "Display options" })).toBeInTheDocument();
  });

  it("works a conversation's More menu, its title switcher and its Share popover", async () => {
    const other = thread({ id: "thr_9", title: "Bump the deps", updated_at: 5 });
    const h = agentsHarness({ entities: [mirrored("project", project(), 1), mirrored("thread", busy, 2), mirrored("thread", other, 3)], follow: ["thr_1"], ctx: { services: fakeServices() } });
    renderAgents(<Conversation thread={busy} machine={machine()} />, h);
    macClick(screen.getByRole("button", { name: "More" }));
    macClick(await screen.findByRole("menuitem", { name: "Rename" }));
    expect(await screen.findByRole("textbox", { name: "Title" })).toBeInTheDocument();
    await userEvent.keyboard("{Escape}");

    macClick(screen.getByRole("button", { name: /Fix the flaky queue test/ }));
    const list = await screen.findByRole("listbox", { name: "Conversations" });
    macClick(within(list).getByRole("option", { name: /Bump the deps/ }));
    expect(h.openThread).toHaveBeenCalledWith("thr_9");

    macClick(screen.getByRole("button", { name: "Share" }));
    const share = await screen.findByRole("dialog", { name: "Share this thread" });
    const toggle = within(share).queryByRole("switch") ?? within(share).queryByRole("checkbox");
    if (toggle) macClick(toggle);
    expect(screen.getByRole("dialog", { name: "Share this thread" })).toBeInTheDocument();
  });

  it("answers a request on its card: an option moves on, Approve sends", async () => {
    const onRespond = async () => {};
    const { unmount } = renderAgents(<RequestCard request={question()} onRespond={onRespond} />, agentsHarness());
    macClick(screen.getByRole("radio", { name: /Postgres/ }));
    expect(await screen.findByText("What should run after the fix?")).toBeInTheDocument();
    unmount();
    const sent: unknown[] = [];
    renderAgents(<RequestCard request={approval()} onRespond={async (r) => void sent.push(r)} />, agentsHarness());
    macClick(screen.getByRole("button", { name: "Approve" }));
    await waitFor(() => expect(sent).toEqual([{ type: "decision", decision: "accept" }]));
  });

  it("picks a row in ⌘K with the search field losing focus to nothing", async () => {
    const h = agentsHarness({ entities, follow: ["thr_1", "thr_2"] });
    renderAgents(<AgentsSurface client={h.client} services={fakeServices()} demo={false} chrome="popup" />, h);
    await userEvent.keyboard("{Meta>}k{/Meta}");
    const palette = await screen.findByRole("dialog", { name: "Command palette" });
    await userEvent.type(within(palette).getByRole("combobox"), "upgr");
    macClick(within(palette).getByRole("option", { name: /Upgrade to Next 16.4/ }));
    await waitFor(() => expect(agentsSurface.get().view).toEqual({ kind: "thread", thread: "thr_2" }));
  });

  it("picks the launcher's project from its select", async () => {
    const two = [mirrored("project", project(), 1), mirrored("project", project({ id: "prj_2", name: "web", path: "/Users/ada/web" }), 2)];
    renderAgents(<Launcher onClose={() => {}} />, agentsHarness({ entities: two }));
    macClick(screen.getByRole("combobox", { name: "Project" }));
    macClick(await screen.findByRole("option", { name: "web" }));
    await waitFor(() => expect(screen.getByRole("combobox", { name: "Project" })).toHaveTextContent("web"));
  });

  it("closing a conversation with a click leaves focus alone (no ring nobody asked for); from the keyboard it goes back to the card", async () => {
    const h = agentsHarness({ entities, follow: ["thr_1", "thr_2"] });
    renderAgents(<AgentsSurface client={h.client} services={fakeServices()} demo={false} chrome="popup" />, h);
    act(() => setAgentsView({ kind: "thread", thread: "thr_2" }));
    macClick(await screen.findByRole("button", { name: "Close the conversation (Esc)" }));
    await waitFor(() => expect(agentsSurface.get().view).toEqual({ kind: "canvas" }));
    await new Promise((r) => setTimeout(r, 20));
    expect(document.activeElement).toBe(document.body);
    act(() => setAgentsView({ kind: "thread", thread: "thr_2" }));
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(document.activeElement).toBe(document.querySelector('[data-card][data-key="t:thr_2"]')));
  });

  it("marks the card a touch landed on, one at a time, so its tools show without focus (Safari doesn't move it on a tap)", () => {
    const h = agentsHarness({ entities, follow: ["thr_1", "thr_2"] });
    renderAgents(<AgentsSurface client={h.client} services={fakeServices()} demo={false} chrome="popup" />, h);
    const slot = (key: string) => document.querySelector<HTMLElement>(`[data-card][data-key="${key}"]`)!;
    fireEvent.pointerDown(slot("m:mch_1"), { pointerType: "touch" });
    expect(slot("m:mch_1")).toHaveAttribute("data-tapped");
    fireEvent.pointerDown(slot("p:prj_1"), { pointerType: "touch" });
    expect(slot("m:mch_1")).not.toHaveAttribute("data-tapped");
    expect(slot("p:prj_1")).toHaveAttribute("data-tapped");
    fireEvent.pointerDown(slot("m:mch_1"), { pointerType: "mouse" });
    expect(slot("m:mch_1")).not.toHaveAttribute("data-tapped");
  });

  it("sends from the composer with its button", async () => {
    const idle = thread({ status: "idle", active_run: null });
    const h = agentsHarness({ entities: [mirrored("project", project(), 1), mirrored("thread", idle, 2)], follow: ["thr_1"] });
    renderAgents(<Conversation thread={idle} machine={machine()} />, h);
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "run the tests");
    macClick(screen.getByRole("button", { name: "Send" }));
    await waitFor(() => expect(h.command).toHaveBeenCalledWith("mch_1", expect.any(String), expect.objectContaining({ type: "message.send", text: "run the tests" })));
  });
});
