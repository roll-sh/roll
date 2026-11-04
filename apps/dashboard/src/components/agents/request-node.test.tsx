import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { NodeBody } from "@/lib/agents-api";
import { RequestNode } from "./nodes";
import { approval, M1, mirrored, node, question, run, thread } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";

const tool = (title: string, kind: "read" | "command" = "read"): NodeBody => ({ type: "tool", name: kind, kind, title, input: null, output: null, exit_code: kind === "command" ? 0 : null, files: [] });

/** A waiting thread: five steps, then the request node. */
function waiting(req: ReturnType<typeof approval>) {
  const t = thread({ status: "waiting", active_run: "run_1", pending_requests: 1 });
  const steps = ["a.rs", "b.rs", "c.rs", "d.rs"].map((f, i) => mirrored("node", node(tool(f), { id: `nod_${i}`, ordinal: i + 1, status: "completed" }), 10 + i));
  return agentsHarness({
    follow: ["thr_1"],
    entities: [
      mirrored("thread", t, 1),
      mirrored("run", run({ status: "waiting" }), 2),
      ...steps,
      mirrored("node", node(tool("cargo test", "command"), { id: "nod_9", ordinal: 5, status: "completed" }), 20),
      mirrored("node", node({ type: "request", request: req.id }, { id: "nod_req", ordinal: 6, status: "waiting" }), 21),
      mirrored("request", { ...req, node: "nod_req" }, 22),
    ],
  });
}

const sent = (h: ReturnType<typeof agentsHarness>) => h.command.mock.calls.at(-1);

describe("RequestNode: answering on the canvas", () => {
  it("shows the last steps that led to the request, the earlier ones a click away", async () => {
    const h = waiting(approval());
    renderAgents(<RequestNode request={approval({ node: "nod_req" })} />, h);
    const steps = screen.getByLabelText("Steps that led here");
    expect(within(steps).getByText("cargo test")).toBeInTheDocument();
    expect(within(steps).getByText("Read d.rs")).toBeInTheDocument();
    expect(within(steps).queryByText("Read a.rs")).toBeNull();
    await userEvent.click(within(steps).getByRole("button", { name: "2 earlier steps" }));
    expect(within(steps).getByText("Read a.rs")).toBeInTheDocument();
    // The thread's name opens the conversation.
    await userEvent.click(screen.getByRole("button", { name: /Fix the flaky queue test/ }));
    expect(h.openThread).toHaveBeenCalledWith("thr_1");
  });

  it("approves right on the card", async () => {
    const h = waiting(approval());
    renderAgents(<RequestNode request={approval({ node: "nod_req" })} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Approve" }));
    expect(sent(h)?.[0]).toBe(M1);
    expect(sent(h)?.[2]).toEqual({ type: "request.respond", request: "req_1", response: { type: "decision", decision: "accept" } });
    expect(h.openThread).not.toHaveBeenCalled();
  });

  it("answers questions right on the card, without opening the conversation", async () => {
    const q = question({ node: "nod_req" });
    const h = waiting(q);
    renderAgents(<RequestNode request={q} />, h);
    const form = screen.getByRole("form", { name: "Answer the agent" });
    fireEvent.keyDown(within(form).getByRole("radio", { name: /SQLite/ }), { key: "1" });
    expect(await within(form).findByText("What should run after the fix?")).toBeInTheDocument();
    await userEvent.click(within(form).getByRole("checkbox", { name: /Unit tests/ }));
    await userEvent.click(within(form).getByRole("button", { name: "Submit answers" }));
    await waitFor(() => expect(sent(h)?.[2]).toEqual({ type: "request.respond", request: "req_2", response: { type: "answers", answers: { db: "SQLite", checks: "Unit tests" } } }));
    expect(h.openThread).not.toHaveBeenCalled();
  });

  it("is read-only in a share: what it asks, and that it waits", () => {
    const h = waiting(approval());
    renderAgents(<RequestNode request={approval({ node: "nod_req" })} />, { ...h, ctx: { ...h.ctx, readOnly: true } });
    expect(screen.getByText("Waiting for an answer…")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Approve" })).toBeNull();
  });
});
