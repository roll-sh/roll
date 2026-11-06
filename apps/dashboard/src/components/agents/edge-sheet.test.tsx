import { render, screen, waitFor } from "@testing-library/react";
import { AnimatePresence } from "motion/react";
import { describe, expect, it } from "vitest";
import { EdgeSheet } from "./edge-sheet";

function Sidebar({ open, phone = false }: { open: boolean; phone?: boolean }) {
  return (
    <AnimatePresence initial={false}>
      {open && (
        <EdgeSheet key="threads" label="Threads" width={268} phone={phone}>
          <p>Fix the flaky test</p>
        </EdgeSheet>
      )}
    </AnimatePresence>
  );
}
const sheet = () => screen.queryByRole("complementary", { name: "Threads", hidden: true });

describe("the threads sidebar's sheet", () => {
  it("slides out inert, keeping what it showed, then unmounts", async () => {
    const { rerender } = render(<Sidebar open />);
    expect(sheet()).toHaveAttribute("data-state", "open");
    rerender(<Sidebar open={false} />);
    expect(sheet()).toHaveAttribute("data-state", "closing");
    expect(sheet()).toHaveAttribute("inert");
    expect(sheet()).toHaveTextContent("Fix the flaky test");
    await waitFor(() => expect(sheet()).toBeNull());
  });

  it("⌘B twice fast turns it around: the same sheet, open, never unmounted", async () => {
    const { rerender } = render(<Sidebar open />);
    const first = sheet();
    rerender(<Sidebar open={false} />);
    rerender(<Sidebar open />);
    expect(sheet()).toBe(first);
    expect(sheet()).toHaveAttribute("data-state", "open");
    expect(sheet()).not.toHaveAttribute("inert");
  });

  it("on a phone it covers the page and leaves the same way", async () => {
    const { rerender } = render(<Sidebar open phone />);
    expect(sheet()).toHaveClass("absolute", "inset-0");
    rerender(<Sidebar open={false} phone />);
    expect(sheet()).toHaveAttribute("data-state", "closing");
    await waitFor(() => expect(sheet()).toBeNull());
  });
});
