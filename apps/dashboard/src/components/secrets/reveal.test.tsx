import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Reveal, REVEAL_MS } from "./ui";

afterEach(() => vi.useRealTimers());

describe("a revealed secret", () => {
  it("masks again after a while and when the window loses focus", async () => {
    vi.useFakeTimers();
    const load = vi.fn(async () => ({ value: "s3cret", problems: [] }));
    render(<Reveal load={load} />);
    const reveal = () => act(async () => void fireEvent.click(screen.getByRole("button", { name: "Reveal the value" })));

    await reveal();
    expect(screen.getByText("s3cret")).toBeTruthy();
    act(() => void vi.advanceTimersByTime(REVEAL_MS));
    expect(screen.queryByText("s3cret")).toBeNull();

    await reveal();
    expect(screen.getByText("s3cret")).toBeTruthy();
    act(() => void window.dispatchEvent(new Event("blur")));
    expect(screen.queryByText("s3cret")).toBeNull();
    expect(load).toHaveBeenCalledTimes(2);
  });
});
