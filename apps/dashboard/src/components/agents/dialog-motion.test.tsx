import { act, fireEvent, render, renderHook, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { AnimatePresence } from "motion/react";
import { useState } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { resetAgentsSurface } from "@/lib/agents-surface";
import { PopupFrame } from "./agents-control";
import { Launcher } from "./new-agent";
import { ThreadsSidebar } from "./threads-sidebar";
import { Dialog, useKept } from "./ui/dialog";
import { mirrored, project, thread } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";

afterEach(() => resetAgentsSurface());

describe("useKept", () => {
  it("keeps the last value while the dialog leaves, until it says it's gone", () => {
    const { result, rerender } = renderHook(({ v }: { v: string | null }) => useKept(v), { initialProps: { v: "a" as string | null } });
    expect(result.current).toMatchObject({ open: true, value: "a" });
    const first = result.current.key;
    rerender({ v: null });
    expect(result.current).toMatchObject({ open: false, value: "a" });
    act(() => result.current.onExited());
    expect(result.current.value).toBeNull();
    // A fresh opening starts over (a new key: its state resets).
    rerender({ v: "b" });
    expect(result.current).toMatchObject({ open: true, value: "b" });
    expect(result.current.key).not.toBe(first);
  });

  it("opened again while it's still leaving, it turns around as it is (same key)", () => {
    const { result, rerender } = renderHook(({ v }: { v: boolean }) => useKept(v), { initialProps: { v: true } });
    const key = result.current.key;
    rerender({ v: false });
    rerender({ v: true });
    expect(result.current).toMatchObject({ open: true, value: true, key });
    // A late "gone" from the first close doesn't drop an open dialog.
    act(() => result.current.onExited());
    expect(result.current.value).toBe(true);
  });
});

function Kept({ initial = true }: { initial?: boolean }) {
  const [on, setOn] = useState(initial);
  const d = useKept(on);
  return (
    <>
      <button onClick={() => setOn((x) => !x)}>toggle</button>
      {d.value && (
        <Dialog key={d.key} open={d.open} onExited={d.onExited} onClose={() => setOn(false)} title="Snooze">
          <p>body</p>
        </Dialog>
      )}
    </>
  );
}

describe("the shared dialog shown for a value", () => {
  it("stays drawn, closing, for as long as its way out plays, then unmounts", async () => {
    // A transition Base UI waits for (jsdom has none: it would finish at once).
    let finish = () => {};
    const finished = new Promise<void>((r) => (finish = r));
    const proto = HTMLElement.prototype as unknown as { getAnimations?: () => unknown[] };
    proto.getAnimations = () => [{ finished, effect: { getTiming: () => ({ duration: 170, iterations: 1 }) }, playState: "running" }];
    try {
      render(<Kept />);
      const dialog = await screen.findByRole("dialog", { name: "Snooze" });
      fireEvent.click(within(dialog).getByRole("button", { name: "Close" }));
      await new Promise((r) => setTimeout(r, 50));
      // Still there, marked as leaving (Base UI's ending style drives the fade and scale).
      expect(dialog).toBeInTheDocument();
      expect(dialog).toHaveAttribute("data-closed");
      expect(dialog).toHaveTextContent("body");
      await act(async () => finish());
      await waitFor(() => expect(screen.queryByRole("dialog", { name: "Snooze" })).toBeNull());
    } finally {
      delete proto.getAnimations;
    }
  });
});

describe("a row's custom snooze", () => {
  it("is hosted beside the sidebar, so it closes on its own even as the row leaves its list", async () => {
    const t = thread({ id: "thr_a", title: "Fix the parser" });
    const h = agentsHarness({ entities: [mirrored("project", project(), 1), mirrored("thread", t, 2)] });
    renderAgents(<ThreadsSidebar data={h.client.data} open setOpen={() => {}} phone={false} openThread={null} />, h);
    const row = screen.getAllByRole("listitem").find((r) => r.hasAttribute("data-row"))!;
    await userEvent.click(within(row).getByRole("button", { name: "Thread actions" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: /Snooze/ }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Custom…" }));
    const dialog = await screen.findByRole("dialog", { name: "Custom snooze" });
    expect(row.contains(dialog)).toBe(false);
    await userEvent.click(within(dialog).getByRole("button", { name: "Duration" }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Snooze" }));
    await waitFor(() => expect(h.command.mock.calls.some(([, , c]) => c.type === "thread.snooze")).toBe(true));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Custom snooze" })).toBeNull());
  });
});

describe("the launcher", () => {
  it("is the shared dialog: named, focus inside, Escape closes it", async () => {
    const onClose = vi.fn();
    const h = agentsHarness({ entities: [mirrored("project", project(), 1)] });
    renderAgents(<Launcher onClose={onClose} />, h);
    const dialog = await screen.findByRole("dialog", { name: "New agent" });
    await waitFor(() => expect(dialog.contains(document.activeElement)).toBe(true));
    await userEvent.keyboard("{Escape}");
    expect(onClose).toHaveBeenCalled();
  });

  it("asked for at another point while open, it's the same dialog moved there, not a new one popping in", async () => {
    const h = agentsHarness({ entities: [mirrored("project", project(), 1)] });
    const { rerender } = renderAgents(<Launcher onClose={vi.fn()} at={{ x: 100, y: 100 }} />, h);
    const dialog = await screen.findByRole("dialog", { name: "New agent" });
    rerender(<Launcher onClose={vi.fn()} at={{ x: 300, y: 200 }} />);
    expect(screen.getByRole("dialog", { name: "New agent" })).toBe(dialog);
    expect(dialog).not.toHaveAttribute("data-starting-style");
  });

  it("closing, it stays drawn and leaves before it unmounts", async () => {
    const h = agentsHarness({ entities: [mirrored("project", project(), 1)] });
    const onExited = vi.fn();
    const { rerender } = renderAgents(<Launcher onClose={vi.fn()} onExited={onExited} />, h);
    const dialog = await screen.findByRole("dialog", { name: "New agent" });
    rerender(<Launcher open={false} onClose={vi.fn()} onExited={onExited} />);
    expect(dialog).toHaveAttribute("data-closed");
    await waitFor(() => expect(onExited).toHaveBeenCalled());
  });
});

function Popup({ open }: { open: boolean }) {
  return <AnimatePresence>{open && <PopupFrame key="agents">surface</PopupFrame>}</AnimatePresence>;
}

describe("the agents popup", () => {
  const frame = () => screen.queryByRole("dialog", { name: "Agents", hidden: true });

  it("leaves inert, still drawn, then unmounts (was gone in a frame)", async () => {
    const { rerender } = render(<Popup open />);
    expect(frame()?.parentElement).toHaveAttribute("data-state", "open");
    rerender(<Popup open={false} />);
    expect(frame()?.parentElement).toHaveAttribute("data-state", "closing");
    expect(frame()?.parentElement).toHaveAttribute("inert");
    expect(frame()).toHaveTextContent("surface");
    await waitFor(() => expect(frame()).toBeNull());
  });

  it("⌘J twice fast turns it around: the same frame, open", () => {
    const { rerender } = render(<Popup open />);
    const first = frame();
    rerender(<Popup open={false} />);
    rerender(<Popup open />);
    expect(frame()).toBe(first);
    expect(first?.parentElement).not.toHaveAttribute("inert");
  });
});
