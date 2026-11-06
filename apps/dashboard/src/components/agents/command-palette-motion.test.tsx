import { act, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { setActiveClient } from "@/lib/agents-client";
import { closePalette, openPalette, resetPalette } from "@/lib/agents-palette";
import { resetAgentsSurface } from "@/lib/agents-surface";
import { mirrored, project, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents } from "@/test/agents-render";
import { AgentsSurface } from "./agents-page";

function setup() {
  const h = agentsHarness({ entities: [mirrored("project", project(), 1), mirrored("thread", thread(), 2)], follow: ["thr_1"] });
  renderAgents(<AgentsSurface client={h.client} services={fakeServices()} demo={false} chrome="popup" />, h);
}
const shells = () => [...document.querySelectorAll<HTMLElement>("[data-palette]")];

beforeEach(() => {
  resetAgentsSurface();
  resetPalette();
});
afterEach(() => setActiveClient(null));

describe("the command palette's motion", () => {
  it("fades out however it closes, inert and hidden meanwhile, then unmounts", async () => {
    setup();
    await userEvent.keyboard("{Control>}k{/Control}");
    expect(await screen.findByRole("dialog", { name: "Command palette" })).toBeInTheDocument();
    await userEvent.keyboard("{Escape}");
    // Still drawn as it goes, but no longer the dialog: a click goes through to the page.
    expect(shells()).toHaveLength(1);
    expect(shells()[0]).toHaveAttribute("data-state", "closing");
    expect(shells()[0]).toHaveAttribute("inert");
    expect(screen.queryByRole("dialog", { name: "Command palette" })).toBeNull();
    await waitFor(() => expect(shells()).toHaveLength(0));
  });

  it("⌘K again while it fades out opens a fresh one: one live palette, the old one finishing its exit", async () => {
    setup();
    act(() => openPalette());
    act(() => closePalette());
    act(() => openPalette());
    expect(screen.getAllByRole("dialog", { name: "Command palette" })).toHaveLength(1);
    expect(shells().filter((s) => s.dataset.state === "open")).toHaveLength(1);
    await waitFor(() => expect(shells()).toHaveLength(1));
    expect(screen.getByRole("dialog", { name: "Command palette" })).toBeInTheDocument();
  });
});
