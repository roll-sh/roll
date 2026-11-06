import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { thread } from "@/test/agents";
import { orchestration, OrchestratorChip } from "./thread-status";

describe("an orchestrator's agents", () => {
  const boss = thread({ id: "thr_boss", orchestrator: true });
  const sub = (id: string, status: "running" | "waiting" | "idle", over: Parameters<typeof thread>[0] = {}) =>
    thread({ id, status, title: `Part ${id}`, lineage: { parent: "thr_boss", relation: "subagent" } as never, ...over });

  it("counts one waiting for you apart from the running ones (the header doesn't call it running either)", () => {
    const counts = orchestration(boss, [boss, sub("a", "running"), sub("b", "waiting"), sub("c", "idle"), thread({ id: "other", status: "running" })]);
    expect(counts).toMatchObject({ started: 3, running: 1, waiting: 1 });
    render(<OrchestratorChip {...counts} />);
    expect(screen.getByLabelText("Orchestrator · 3 agents, 1 running, 1 waiting for you")).toHaveTextContent("Orchestrator3 · 1 running · 1 waiting");
  });

  it("says nothing runs when the only one left waits", () => {
    const counts = orchestration(boss, [sub("b", "waiting")]);
    expect(counts).toMatchObject({ started: 1, running: 0, waiting: 1 });
  });

  it("opens a finished subagent from the chip (it has left the canvas), and the click doesn't reach the card under it", async () => {
    const counts = orchestration(boss, [sub("b", "idle", { created_at: 2 }), sub("a", "idle", { created_at: 1, error: "boom" }), sub("c", "running", { created_at: 3 })]);
    expect(counts.agents.map((t) => t.id)).toEqual(["a", "b", "c"]);
    const open = vi.fn();
    const card = vi.fn();
    render(
      <div onClick={card}>
        <OrchestratorChip {...counts} onOpen={open} />
      </div>,
    );
    await userEvent.click(screen.getByRole("button", { name: "Orchestrator · 3 agents, 1 running: open one" }));
    const items = await screen.findAllByRole("menuitem");
    expect(items.map((i) => i.getAttribute("aria-label"))).toEqual(["Part a, Failed", "Part b, Done", "Part c, Working"]);
    await userEvent.click(screen.getByRole("menuitem", { name: "Part b, Done" }));
    expect(open).toHaveBeenCalledWith("b");
    expect(card).not.toHaveBeenCalled();
  });

  it("stays a plain mark when it started nothing", () => {
    render(<OrchestratorChip {...orchestration(boss, [])} onOpen={() => {}} />);
    expect(screen.queryByRole("button")).toBeNull();
    expect(screen.getByLabelText("Orchestrator")).toBeTruthy();
  });
});
