import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Reveal, REVEAL_MS, TotpCode } from "./ui";

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

describe("a TOTP key's code", () => {
  it("hides when its step ends", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(59_000);
    // RFC 6238's SHA1 vector at T = 59, its step ending at 60 s.
    const load = vi.fn(async () => ({ code: "287082", expires_at: 60_000, period: 30 }));
    render(<TotpCode load={load} />);
    await act(async () => void fireEvent.click(screen.getByRole("button", { name: "Show the one-time code" })));
    expect(screen.getByText("287082")).toBeTruthy();
    act(() => void vi.advanceTimersByTime(1_000));
    expect(screen.queryByText("287082")).toBeNull();
  });
});
