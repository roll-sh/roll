import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { mirrored, node, run, thread } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";
import { RunView } from "./transcript";

// What the daemon gives an orchestrator when its delegated tasks finish (`tasks.rs`, `wake_text`).
const wake = run({
  id: "run_wake",
  ordinal: 2,
  status: "completed",
  prompt: "Delegated task nod_0000000000000001 (Build) [completed]:\nDone: the build passes.\n\nDelegated task nod_0000000000000002 (Docs) [failed]:\nthe task asked to fail",
  delivers: ["nod_0000000000000001", "nod_0000000000000002"],
});

describe("a run that delivers delegated tasks' results (Run.delivers)", () => {
  it("shows in the conversation as a finished-task row, not the user's message; the results open under it", async () => {
    renderAgents(<RunView run={wake} tree={[]} requests={new Map()} checkpoint={undefined} now={Date.now()} actions={{ openThread: vi.fn() }} busyThread={false} />, agentsHarness());
    const row = screen.getByRole("button", { name: "2 delegated tasks finished · Build, Docs failed" });
    expect(screen.queryByRole("button", { name: "Copy the message" })).toBeNull();
    expect(row).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(row);
    expect(row).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText(/Done: the build passes\./)).toBeInTheDocument();
  });

  it("links each task to its subagent's thread", async () => {
    const sub = (id: string, child: string) => node({ type: "subagent", agent: "roll", prompt: "go", child, result: null }, { id, status: "completed" });
    const h = agentsHarness({ follow: ["thr_1"], entities: [mirrored("thread", thread(), 1), mirrored("node", sub("nod_0000000000000001", "thr_build"), 2), mirrored("node", sub("nod_0000000000000002", "thr_docs"), 3)] });
    const openThread = vi.fn();
    renderAgents(<RunView run={wake} tree={[]} requests={new Map()} checkpoint={undefined} now={Date.now()} actions={{ openThread }} busyThread={false} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Open Docs" }));
    expect(openThread).toHaveBeenCalledWith("thr_docs");
    expect(screen.getByRole("button", { name: "Open Build" })).toBeInTheDocument();
  });

});
