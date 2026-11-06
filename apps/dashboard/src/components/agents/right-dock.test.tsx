import { act, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { RightDock, type DockPane } from "./right-dock";

const conversation: DockPane = { key: "thread:thr_1", label: "Conversation", node: <p>Fix the flaky test</p> };
const automations: DockPane = { key: "automations", label: "Automations", node: <p>Morning triage</p> };
const dock = () => document.querySelector<HTMLElement>("aside[data-panel]");
const wait = (ms: number) => act(() => new Promise((r) => setTimeout(r, ms)));

function reduceMotion(on: boolean) {
  vi.spyOn(window, "matchMedia").mockImplementation(
    (query: string) =>
      ({
        matches: on && query.includes("reduce"),
        media: query,
        addEventListener: () => {},
        removeEventListener: () => {},
      }) as unknown as MediaQueryList,
  );
}

afterEach(() => vi.restoreAllMocks());

describe("the right dock", () => {
  it("slides out before it unmounts, inert meanwhile so clicks reach the canvas", async () => {
    const { rerender } = render(<RightDock pane={conversation} phone={false} />);
    expect(screen.getByRole("complementary", { name: "Conversation" })).toHaveAttribute("data-state", "open");
    rerender(<RightDock pane={null} phone={false} />);
    // Still there, leaving: what was in it stays put while it goes.
    expect(dock()).toHaveAttribute("data-state", "closing");
    expect(dock()).toHaveAttribute("inert");
    expect(dock()).toHaveTextContent("Fix the flaky test");
    // (How long it takes is the browser's to show: jsdom has no layout, so motion ends at once.)
    await waitFor(() => expect(dock()).toBeNull());
  });

  it("opening again while it slides out turns it around: the same frame, never unmounted", async () => {
    const { rerender } = render(<RightDock pane={conversation} phone={false} />);
    const frame = dock();
    rerender(<RightDock pane={null} phone={false} />);
    expect(dock()).toHaveAttribute("data-state", "closing");
    rerender(<RightDock pane={automations} phone={false} />);
    expect(dock()).toBe(frame);
    expect(dock()).toHaveAttribute("data-state", "open");
    expect(dock()).not.toHaveAttribute("inert");
    await wait(400);
    expect(dock()).toBe(frame);
    expect(screen.getByRole("complementary", { name: "Automations" })).toHaveTextContent("Morning triage");
  });

  it("switching what's open crossfades in the same frame: the old pane fades out hidden, the new one is live", async () => {
    const { rerender } = render(<RightDock pane={conversation} phone={false} />);
    const frame = dock();
    rerender(<RightDock pane={automations} phone={false} />);
    expect(dock()).toBe(frame);
    expect(frame).toHaveAttribute("aria-label", "Automations");
    const leaving = frame!.querySelector<HTMLElement>('[data-pane="leaving"]');
    expect(leaving).toHaveTextContent("Fix the flaky test");
    expect(leaving).toHaveAttribute("inert");
    expect(leaving).toHaveAttribute("aria-hidden", "true");
    expect(frame!.querySelector('[data-pane="current"]')).toHaveTextContent("Morning triage");
    await waitFor(() => expect(frame!.querySelectorAll("[data-pane]")).toHaveLength(1));
    expect(screen.queryByText("Fix the flaky test")).toBeNull();
  });

  it("with reduced motion it goes at once (no slide), and still follows every change", async () => {
    reduceMotion(true);
    const { rerender } = render(<RightDock pane={conversation} phone={false} />);
    rerender(<RightDock pane={automations} phone={false} />);
    await wait(40);
    expect(screen.getByRole("complementary", { name: "Automations" }).querySelectorAll("[data-pane]")).toHaveLength(1);
    rerender(<RightDock pane={null} phone={false} />);
    await wait(40);
    expect(dock()).toBeNull();
  });

  it("on a phone it covers the canvas and leaves the same way", async () => {
    const { rerender } = render(<RightDock pane={conversation} phone />);
    expect(dock()).toHaveClass("absolute", "inset-0");
    rerender(<RightDock pane={null} phone />);
    expect(dock()).toHaveAttribute("data-state", "closing");
    await waitFor(() => expect(dock()).toBeNull());
  });
});
