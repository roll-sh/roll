import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { deliveredLabel } from "@/lib/agents-worklog";
import { machine, mirrored, run, thread } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";
import { ThreadNode } from "./nodes";
import { QueuedMessages } from "./queue";
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
  it("is called what it is: each task and how it ended", () => {
    expect(deliveredLabel(wake)).toBe("2 delegated tasks finished · Build, Docs failed");
    expect(deliveredLabel({ prompt: "Delegated task nod_9 (Tests) [completed]:\nok", delivers: ["nod_9"] })).toBe("Delegated task finished · Tests");
    // A result that didn't fit is only pointed at; a task the text doesn't name still counts.
    expect(deliveredLabel({ prompt: "", delivers: ["nod_x"] })).toBe("Delegated task finished · A task");
  });

  it("shows in the conversation as a finished-task row, not the user's message; the results open under it", async () => {
    renderAgents(<RunView run={wake} tree={[]} requests={new Map()} checkpoint={undefined} now={Date.now()} actions={{ openThread: vi.fn() }} busyThread={false} />, agentsHarness());
    const row = screen.getByRole("button", { name: "2 delegated tasks finished · Build, Docs failed" });
    expect(screen.queryByRole("button", { name: "Copy the message" })).toBeNull();
    expect(row).toHaveAttribute("aria-expanded", "false");
    await userEvent.click(row);
    expect(row).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText(/Done: the build passes\./)).toBeInTheDocument();
  });

  it("is a person's message when it delivers nothing", () => {
    renderAgents(<RunView run={run({ status: "completed", prompt: "Ship dark mode" })} tree={[]} requests={new Map()} checkpoint={undefined} now={Date.now()} actions={{ openThread: vi.fn() }} busyThread={false} />, agentsHarness());
    expect(screen.getByText("Ship dark mode")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Copy the message" })).toBeInTheDocument();
  });

  it("names its tasks on the orchestrator's card instead of the wake's text", () => {
    const t = thread({ orchestrator: true, run_count: 2, title: "Ship dark mode" });
    renderAgents(<ThreadNode thread={t} machine={machine()} />, agentsHarness({ entities: [mirrored("thread", t, 1), mirrored("run", run({ status: "completed", prompt: "Ship dark mode" }), 2), mirrored("run", wake, 3)] }));
    const card = screen.getByRole("button", { name: "Thread Ship dark mode" });
    expect(within(card).getByText("2 delegated tasks finished · Build, Docs failed")).toBeInTheDocument();
    expect(within(card).queryByText(/Delegated task nod_/)).toBeNull();
  });

  it("waits in the queue as results, which aren't the user's to edit", () => {
    const actions = { edit: vi.fn(), remove: vi.fn(), move: vi.fn(), sendNow: vi.fn() };
    renderAgents(
      <QueuedMessages
        queued={[
          { id: "q1", text: "then run clippy", attachments: [], at: 0 },
          { id: "q2", text: wake.prompt, attachments: [], at: 0, delivers: wake.delivers },
        ]}
        actions={actions}
        running
        held={false}
        editing={null}
        onResume={vi.fn()}
      />,
      agentsHarness(),
    );
    const results = screen.getByRole("listitem", { name: "Queued: 2 delegated tasks finished · Build, Docs failed" });
    expect(within(results).queryByRole("button", { name: "Edit in the composer" })).toBeNull();
    expect(within(screen.getByRole("listitem", { name: "Queued: then run clippy" })).getByRole("button", { name: "Edit in the composer" })).toBeInTheDocument();
  });
});
