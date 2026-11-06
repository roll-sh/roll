import { render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { SlidePages } from "./slide-pages";

function Pages({ page, dir, text }: { page: string; dir: -1 | 0 | 1; text?: string }) {
  return (
    <SlidePages page={page} direction={dir}>
      <p>{text ?? page}</p>
    </SlidePages>
  );
}
const current = () => document.querySelector("[data-page=current]");
const leaving = () => document.querySelector("[data-page=leaving]");

describe("pages replacing each other (the palette's views, the automations stack)", () => {
  it("a page deeper comes from the right while the old one leaves, inert and hidden, then goes", async () => {
    const { rerender } = render(<Pages page="list" dir={0} />);
    expect(current()).not.toHaveAttribute("data-from");
    rerender(<Pages page="overview" dir={1} />);
    expect(current()).toHaveAttribute("data-from", "right");
    expect(current()).toHaveTextContent("overview");
    // The old page is still drawn, keeping what it showed, but out of reach.
    expect(leaving()).toHaveTextContent("list");
    expect(leaving()).toHaveAttribute("inert");
    expect(leaving()).toHaveAttribute("aria-hidden", "true");
    expect(screen.getByText("overview")).toBeInTheDocument();
    await waitFor(() => expect(leaving()).toBeNull());
  });

  it("Back comes from the left", async () => {
    const { rerender } = render(<Pages page="overview" dir={0} />);
    rerender(<Pages page="list" dir={-1} />);
    expect(current()).toHaveAttribute("data-from", "left");
    await waitFor(() => expect(leaving()).toBeNull());
  });

  it("back and forth fast turns the same page around instead of mounting another", () => {
    const { rerender } = render(<Pages page="list" dir={0} />);
    const list = current();
    rerender(<Pages page="overview" dir={1} />);
    rerender(<Pages page="list" dir={-1} />);
    expect(current()).toBe(list);
    expect(list).not.toHaveAttribute("inert");
  });

  it("a change that keeps the page (a tab on the same level) doesn't move it", () => {
    const { rerender } = render(<Pages page="builder" dir={1} text="Build" />);
    const page = current();
    rerender(<Pages page="builder" dir={0} text="Settings" />);
    expect(current()).toBe(page);
    expect(page).toHaveTextContent("Settings");
    expect(leaving()).toBeNull();
  });
});
