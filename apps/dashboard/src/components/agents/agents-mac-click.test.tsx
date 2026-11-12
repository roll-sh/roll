/**
 * The agents surface under a macOS click (Safari, Firefox on a Mac): a button, link or menu row
 * never takes focus, and what had it blurs with no `relatedTarget`. Chrome and Linux WebKit focus
 * the button, so these paths only break there; `macClick` (src/test/mac-click.ts) makes them here.
 */
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { setActiveClient } from "@/lib/agents-client";
import { agentsSurface, resetAgentsSurface } from "@/lib/agents-surface";
import { approval, mirrored, node, project, run, terminal, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";
import { macClick } from "@/test/mac-click";
import { AgentsSurface } from "./agents-page";

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
  it("picks a row in ⌘K with the search field losing focus to nothing", async () => {
    const h = agentsHarness({ entities, follow: ["thr_1", "thr_2"] });
    renderAgents(<AgentsSurface client={h.client} services={fakeServices()} demo={false} chrome="popup" />, h);
    await userEvent.keyboard("{Meta>}k{/Meta}");
    const palette = await screen.findByRole("dialog", { name: "Command palette" });
    await userEvent.type(within(palette).getByRole("combobox"), "upgr");
    macClick(within(palette).getByRole("option", { name: /Upgrade to Next 16.4/ }));
    await waitFor(() => expect(agentsSurface.get().view).toEqual({ kind: "thread", thread: "thr_2" }));
  });
});
