import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { thread } from "@/test/agents";
import { orchestration, OrchestratorChip } from "./thread-status";

describe("an orchestrator's agents", () => {
  const boss = thread({ id: "thr_boss", orchestrator: true });
  const sub = (id: string, status: "running" | "waiting" | "idle") => thread({ id, status, lineage: { parent: "thr_boss", relation: "subagent" } as never });

  it("counts one waiting for you apart from the running ones (the header doesn't call it running either)", () => {
    const counts = orchestration(boss, [boss, sub("a", "running"), sub("b", "waiting"), sub("c", "idle"), thread({ id: "other", status: "running" })]);
    expect(counts).toEqual({ started: 3, running: 1, waiting: 1 });
    render(<OrchestratorChip {...counts} />);
    expect(screen.getByLabelText("Orchestrator · 3 agents, 1 running, 1 waiting for you")).toHaveTextContent("Orchestrator3 · 1 running · 1 waiting");
  });

  it("says nothing runs when the only one left waits", () => {
    const counts = orchestration(boss, [sub("b", "waiting")]);
    expect(counts).toEqual({ started: 1, running: 0, waiting: 1 });
  });
});
