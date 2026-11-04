import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { approval, machine, mirrored, node, project, run, share, thread } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";
import { MarkStateProvider } from "../mark-state";

const agents = vi.hoisted(() => ({ createShare: vi.fn(), shares: vi.fn(), revokeShare: vi.fn() }));
vi.mock("@/lib/agents-api", async (orig) => ({ ...(await orig()), agentsApi: agents }));

import { ShareButton, SharesList } from "./share";
import { clock, ShareView } from "./share-view";
import { Conversation } from "./session";

const view = (s = share()) => render(<ShareView share={s} />, { wrapper: MarkStateProvider });

/** `prefers-reduced-motion: reduce` on or off for the test. */
function reducedMotion(on: boolean) {
  const mm = window.matchMedia;
  window.matchMedia = ((query: string) => ({ ...mm(query), matches: on && query.includes("reduce"), addEventListener: () => {}, removeEventListener: () => {} })) as typeof window.matchMedia;
  return () => {
    window.matchMedia = mm;
  };
}

const summary = { id: "shr_1", title: "Fix the flaky queue test", thread: "thr_1", subtree: false, url: "https://roll.sh/agents/s/shr_1", threads: 1, runs: 2, bytes: 18_432, created_at: 0, created_by: "acme" };

describe("ShareView", () => {
  let restore = () => {};
  afterEach(() => restore());

  it("with reduced motion, shows the share as frozen, read-only, and steps through its states", async () => {
    restore = reducedMotion(true);
    view();
    expect(screen.getByRole("heading", { level: 1, name: "Fix the flaky queue test" })).toBeInTheDocument();
    expect(screen.getByText(/Shared by/)).toHaveTextContent("Shared by acme · ada-mbp (macOS) · 1 run · 3 steps · 2 threads");
    // The final frame: the whole answer, the approval as decided, nothing to send or answer.
    expect(screen.getByText("0123456789")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Approve" })).toBeNull();
    expect(screen.queryByRole("textbox")).toBeNull();
    expect(screen.queryByRole("button", { name: "Share" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Delete" })).toBeNull();
    // Back to the start: the prompt was just sent, nothing answered yet.
    await userEvent.click(screen.getByRole("button", { name: "Back to the start" }));
    expect(screen.getByText("Find out why the queue test is flaky")).toBeInTheDocument();
    expect(screen.queryByText("0123456789")).toBeNull();
    expect(screen.getByText(/^0:00 \//)).toBeInTheDocument();
    // Step by step to the approval: it waits, and can't be answered here.
    for (let i = 0; i < 5; i++) await userEvent.click(screen.getByRole("button", { name: "Next step" }));
    expect(screen.getByText("Find out why the queue test is flaky")).toBeInTheDocument();
    expect(screen.getByText("0123456789")).toBeInTheDocument();
    expect(screen.getAllByText("Waiting for an answer…").length).toBeGreaterThan(0);
    expect(screen.queryByRole("button", { name: "Approve" })).toBeNull();
  });

  it("rewinds and plays the run back, streaming its text", async () => {
    restore = reducedMotion(false);
    view();
    // It rewinds at once and starts playing shortly after.
    await waitFor(() => expect(screen.queryByText("0123456789")).toBeNull());
    fireEvent.change(screen.getByRole("combobox", { name: "Speed" }), { target: { value: "8" } });
    expect(await screen.findByRole("button", { name: "Pause" }, { timeout: 2000 })).toBeInTheDocument();
    // It ends on the share as frozen, and offers to replay.
    expect(await screen.findByRole("button", { name: "Replay" }, { timeout: 6000 })).toBeInTheDocument();
    expect(screen.getByText("0123456789")).toBeInTheDocument();
  }, 15_000);

  it("scrubs to any moment", async () => {
    restore = reducedMotion(true);
    view();
    const scrub = screen.getByRole("slider", { name: "Replay position" });
    act(() => {
      fireEvent.change(scrub, { target: { value: "0" } });
    });
    expect(screen.queryByText("0123456789")).toBeNull();
    act(() => {
      fireEvent.change(scrub, { target: { value: scrub.getAttribute("max")! } });
    });
    expect(screen.getByText("0123456789")).toBeInTheDocument();
  });

  it("formats the clock", () => {
    expect([clock(0), clock(61_400), clock(-5)]).toEqual(["0:00", "1:01", "0:00"]);
  });
});

describe("Conversation, read-only", () => {
  it("is the transcript alone: no composer, actions or answers", () => {
    const t = thread({ status: "waiting", active_run: "run_1", run_count: 1, pending_requests: 1 });
    const h = agentsHarness({
      follow: ["thr_1"],
      entities: [mirrored("project", project(), 1), mirrored("thread", t, 2), mirrored("run", run({ status: "waiting" }), 3), mirrored("node", node({ type: "request", request: "req_1" }, { id: "nod_d", status: "waiting" }), 4), mirrored("request", approval({ node: "nod_d" }), 5)],
    });
    h.ctx.readOnly = true;
    renderAgents(<Conversation thread={t} machine={machine()} />, h);
    expect(screen.getByText("Run `cargo test -p code-control`")).toBeInTheDocument();
    expect(screen.getByText("Waiting for an answer…")).toBeInTheDocument();
    for (const name of ["Approve", "Share", "More", "Diff", "Fork this conversation"]) expect(screen.queryByRole("button", { name })).toBeNull();
    expect(screen.queryByRole("textbox")).toBeNull();
  });
});

describe("ShareButton", () => {
  beforeEach(() => Object.values(agents).forEach((f) => f.mockReset()));

  it("makes a link to the thread", async () => {
    agents.createShare.mockResolvedValue(summary);
    const h = agentsHarness({ entities: [mirrored("thread", thread(), 1)] });
    renderAgents(<ShareButton thread={thread()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Share" }));
    const dialog = screen.getByRole("dialog", { name: "Share this thread" });
    // No subagents: nothing to include.
    expect(within(dialog).queryByRole("checkbox")).toBeNull();
    await userEvent.click(within(dialog).getByRole("button", { name: "Create link" }));
    expect(agents.createShare).toHaveBeenCalledWith("acme", "thr_1", false);
    expect(await within(dialog).findByText(`${window.location.origin}/agents/s/shr_1`)).toBeInTheDocument();
    expect(within(dialog).getByRole("link", { name: "Open the share" })).toHaveAttribute("href", "/agents/s/shr_1");
    expect(within(dialog).getByRole("link", { name: "Manage shares" })).toHaveAttribute("href", "/dashboard/agents/shares?owner=acme");
    await userEvent.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("can include the subagent threads, and shows a refusal", async () => {
    agents.createShare.mockRejectedValue(new Error("this thread is too large to share (5 MB; at most 4 MB)"));
    const child = thread({ id: "thr_2", lineage: { parent: "thr_1", relation: "subagent", root: "thr_1", from_run: null } });
    const h = agentsHarness({ entities: [mirrored("thread", thread(), 1), mirrored("thread", child, 2)] });
    renderAgents(<ShareButton thread={thread()} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Share" }));
    expect(screen.getByRole("checkbox", { name: /subagent threads/ })).toBeChecked();
    await userEvent.click(screen.getByRole("button", { name: "Create link" }));
    expect(agents.createShare).toHaveBeenCalledWith("acme", "thr_1", true);
    expect(await screen.findByRole("alert")).toHaveTextContent("too large to share");
  });
});

describe("SharesList", () => {
  beforeEach(() => Object.values(agents).forEach((f) => f.mockReset()));

  it("lists an owner's links and revokes one after asking", async () => {
    agents.shares.mockResolvedValue([summary, { ...summary, id: "shr_2", title: "Second", threads: 3 }]);
    agents.revokeShare.mockResolvedValue(undefined);
    render(<SharesList owner="acme" />);
    const list = await screen.findByRole("list", { name: "Shares" });
    expect(agents.shares).toHaveBeenCalledWith("acme");
    expect(within(list).getByRole("link", { name: "Second" })).toHaveAttribute("href", "/agents/s/shr_2");
    expect(within(list).getByText(/3 threads · 2 runs · 18k B · by acme/)).toBeInTheDocument();
    await userEvent.click(within(list).getByRole("button", { name: "Revoke Second" }));
    await userEvent.click(within(list).getByRole("button", { name: "Revoke link" }));
    expect(agents.revokeShare).toHaveBeenCalledWith("acme", "shr_2");
    expect(within(list).queryByText("Second")).toBeNull();
  });

  it("says when nothing is shared", async () => {
    agents.shares.mockResolvedValue([]);
    render(<SharesList owner="acme" />);
    expect(await screen.findByText(/Nothing shared yet/)).toBeInTheDocument();
  });
});
