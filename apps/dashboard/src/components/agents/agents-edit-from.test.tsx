import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";
import { Conversation } from "./session";
import { checkpoint, machine, mirrored, node, project, run, thread } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";

// T3's "Edit from here" (`TimelineUserMessageRow.tsx`, `useTimelineRewind.ts`): a sent message goes
// back to before it (files and conversation) and returns to the composer, appended to the draft.

const idle = thread({ id: "thr_1", status: "idle", run_count: 2 });
const runs = [
  run({ id: "run_1", ordinal: 1, prompt: "Write the parser", status: "completed", completed_at: 2, checkpoint: "chk_1" }),
  run({ id: "run_2", ordinal: 2, prompt: "Now add tests", status: "completed", completed_at: 3, checkpoint: "chk_2" }),
];
const checkpoints = [
  checkpoint({ id: "chk_0", run: null, ordinal: 0 }),
  checkpoint({ id: "chk_1", run: "run_1", ordinal: 1 }),
  checkpoint({ id: "chk_2", run: "run_2", ordinal: 2 }),
];

function world(t = idle) {
  const entities = [mirrored("project", project(), 1), mirrored("thread", t, 2), ...runs.map((r, i) => mirrored("run", r, 3 + i)), ...checkpoints.map((c, i) => mirrored("checkpoint", c, 6 + i)),
    ...runs.map((r, i) => mirrored("node", node({ type: "message", text: `Done: ${r.prompt}` }, { id: `nod_${i}`, run: r.id, status: "completed" }), 10 + i)),
  ];
  return agentsHarness({ entities, follow: ["thr_1"] });
}

afterEach(() => localStorage.clear());

describe("Edit from here", () => {
  it("asks, rolls back to the checkpoint before the message, and puts it back after what's typed", async () => {
    const h = world();
    renderAgents(<Conversation thread={idle} machine={machine()} />, h);
    const box = screen.getByRole("textbox", { name: /message/i });
    await userEvent.type(box, "also check the docs");
    await userEvent.click(screen.getAllByRole("button", { name: "Edit from here" })[1]);
    expect(h.command).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Go back and edit" }));
    await waitFor(() => expect(h.command.mock.calls.map((c) => c[2])).toContainEqual({ type: "checkpoint.rollback", thread: "thr_1", checkpoint: "chk_1" }));
    await waitFor(() => expect(box).toHaveValue("also check the docs\n\nNow add tests"));
  });

});
