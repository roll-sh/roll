import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { Composer } from "./composer";
import { machine, provider, thread } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";

describe("the composer's hint", () => {
  const caps = provider().capabilities;
  const on = (capabilities: typeof caps) => machine({ info: { ...machine().info!, providers: [provider({ capabilities })] } });

  it("offers / only when the provider has commands (a fake with none said '/ for commands' and / showed nothing)", () => {
    const { unmount } = renderAgents(<Composer mode={{ kind: "thread", thread: thread(), machine: on(caps) }} />, agentsHarness());
    expect(screen.getByRole("textbox", { name: "Message" })).toHaveAttribute("placeholder", "Ask anything. / for commands");
    unmount();
    renderAgents(<Composer mode={{ kind: "thread", thread: thread(), machine: on({ ...caps, plan_mode: false, compact: false }) }} />, agentsHarness());
    expect(screen.getByRole("textbox", { name: "Message" })).toHaveAttribute("placeholder", "Ask anything");
  });
});
