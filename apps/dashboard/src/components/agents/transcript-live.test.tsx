import { screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { approval, run } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";
import { RunView } from "./transcript";

describe("a live run's line", () => {
  it("counts a wait from the request it waits on, not from the run's start (asked again after a steer, it said minutes at once)", () => {
    const now = Date.now();
    const r = run({ id: "run_1", status: "waiting", started_at: now - 277_000 });
    const asked = approval({ id: "req_2", run: "run_1", created_at: now - 4_000 });
    const answered = approval({ id: "req_1", run: "run_1", status: "resolved", created_at: now - 270_000 });
    renderAgents(
      <RunView run={r} tree={[]} requests={new Map([["req_1", answered], ["req_2", asked]])} checkpoint={undefined} now={now} actions={{ openThread: vi.fn() }} busyThread last />,
      agentsHarness(),
    );
    expect(screen.getByText(/^Waiting for you for 4s$/)).toBeInTheDocument();
  });

  it("counts work from the run's start", () => {
    const now = Date.now();
    renderAgents(<RunView run={run({ id: "run_1", status: "running", started_at: now - 65_000 })} tree={[]} requests={new Map()} checkpoint={undefined} now={now} actions={{}} busyThread last />, agentsHarness());
    expect(screen.getByText(/^Working for 1m 0?5s$/)).toBeInTheDocument();
  });
});
