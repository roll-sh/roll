import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { Menu } from "./menu";

describe("the small actions menu", () => {
  it("asks before a dangerous action; closed and opened again it asks afresh (disarmed as it opens, so the label never flips while it fades)", async () => {
    const remove = vi.fn();
    render(<Menu label="More" trigger="⋯" items={[{ label: "Delete", confirm: "Delete thread?", danger: true, onSelect: remove }]} />);
    await userEvent.click(screen.getByRole("button", { name: "More" }));
    await userEvent.click(await screen.findByRole("menuitem", { name: "Delete" }));
    expect(screen.getByRole("menuitem", { name: "Delete thread?" })).toBeInTheDocument();
    expect(remove).not.toHaveBeenCalled();
    await userEvent.keyboard("{Escape}");
    await userEvent.click(screen.getByRole("button", { name: "More" }));
    expect(await screen.findByRole("menuitem", { name: "Delete" })).toBeInTheDocument();
    await userEvent.click(screen.getByRole("menuitem", { name: "Delete" }));
    await userEvent.click(screen.getByRole("menuitem", { name: "Delete thread?" }));
    expect(remove).toHaveBeenCalledOnce();
  });
});
