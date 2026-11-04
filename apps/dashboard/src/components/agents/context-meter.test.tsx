import { screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { Composer } from "./composer";
import { ContextMeter } from "./context-meter";
import { ThreadNode } from "./nodes";
import { machine, mirrored, thread } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";
import { render } from "@testing-library/react";

const ring = (el: HTMLElement) => el.querySelectorAll("circle")[1];

describe("ContextMeter", () => {
  it("fills the ring with what's used and says it on hover", () => {
    render(<ContextMeter context={{ used_tokens: 124_000, max_tokens: 200_000, total_processed_tokens: 410_000 }} label />);
    const meter = screen.getByRole("img", { name: "Context window 62% used" });
    expect(meter).toHaveTextContent("62%");
    const tip = within(meter).getByRole("tooltip", { hidden: true });
    expect(tip).toHaveTextContent("Context window62% · 124k/200k");
    expect(tip).toHaveTextContent("Total processed410k");
    expect(ring(meter)).toHaveClass("stroke-muted");
    expect(Number(ring(meter).getAttribute("stroke-dashoffset"))).toBeCloseTo(2 * Math.PI * 9.75 * 0.38, 3);
  });

  it("turns accent only when nearly full", () => {
    render(<ContextMeter context={{ used_tokens: 235_000, max_tokens: 258_400, total_processed_tokens: null }} label />);
    const meter = screen.getByRole("img", { name: "Context window 91% used" });
    expect(ring(meter)).toHaveClass("stroke-accent");
    expect(within(meter).getByText("91%")).toHaveClass("text-accent");
  });

  it("shows tokens, never a percentage, when the window is unknown", () => {
    render(<ContextMeter context={{ used_tokens: 8_500, max_tokens: null, total_processed_tokens: null }} label />);
    const meter = screen.getByRole("img", { name: "Context window 8.5k tokens used" });
    expect(meter).not.toHaveTextContent("%");
    expect(within(meter).getByRole("tooltip", { hidden: true })).toHaveTextContent("The model's window isn't known.");
  });
});

describe("context on the canvas and in the composer", () => {
  const context = { used_tokens: 124_000, max_tokens: 200_000, total_processed_tokens: null };

  it("ends the thread card's footer, after the token count", () => {
    const t = thread({ run_count: 1, usage: { input_tokens: 12_000, output_tokens: 3_400, cached_tokens: 0, cost_usd: null }, context });
    renderAgents(<ThreadNode thread={t} machine={machine()} />, agentsHarness({ entities: [mirrored("thread", t, 1)] }));
    const card = screen.getByRole("button", { name: "Thread Fix the flaky queue test" });
    const meter = within(card).getByRole("img", { name: "Context window 62% used" });
    const footer = meter.parentElement!;
    expect(footer.lastElementChild).toBe(meter);
    expect(footer).toHaveTextContent(/15k tok.*62%/);
  });

  it("leaves a thread with no report alone", () => {
    renderAgents(<ThreadNode thread={thread()} machine={machine()} />, agentsHarness());
    expect(screen.queryByRole("img", { name: /Context window/ })).toBeNull();
  });

  it("sits in the composer's footer next to send", () => {
    renderAgents(<Composer mode={{ kind: "thread", thread: thread({ context }), machine: machine() }} />, agentsHarness());
    const meter = screen.getByRole("img", { name: "Context window 62% used" });
    const send = screen.getByRole("button", { name: "Send" });
    expect(meter.closest("span.ml-auto")).toBe(send.closest("span.ml-auto"));
  });
});
