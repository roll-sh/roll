import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import { TerminalKeys, withCtrl } from "./terminal-keys";

function Keys({ send }: { send: (d: string) => void }) {
  const [ctrl, setCtrl] = useState(false);
  return <TerminalKeys send={send} ctrl={ctrl} setCtrl={setCtrl} />;
}

describe("a phone terminal's key row", () => {
  it("sends Esc, Tab, ^C and arrows, and a sticky Ctrl applies to the next key only", async () => {
    const send = vi.fn();
    render(<Keys send={send} />);
    await userEvent.click(screen.getByRole("button", { name: "Escape" }));
    await userEvent.click(screen.getByRole("button", { name: "Tab" }));
    await userEvent.click(screen.getByRole("button", { name: "Control C" }));
    await userEvent.click(screen.getByRole("button", { name: "Up" }));
    expect(send.mock.calls.map((c) => c[0])).toEqual(["\x1b", "\t", "\x03", "\x1b[A"]);
    const ctrl = screen.getByRole("button", { name: "Control" });
    await userEvent.click(ctrl);
    expect(ctrl).toHaveAttribute("aria-pressed", "true");
    await userEvent.click(screen.getByRole("button", { name: "Pipe" }));
    expect(ctrl).toHaveAttribute("aria-pressed", "false");
    // A key never takes focus from the terminal (the phone's keyboard stays up).
    expect(fireEvent.pointerDown(screen.getByRole("button", { name: "Left" }))).toBe(false);
  });

  it("Ctrl turns a letter into its control character", () => {
    expect(withCtrl("c")).toBe("\x03");
    expect(withCtrl("D")).toBe("\x04");
    expect(withCtrl("[")).toBe("\x1b");
    expect(withCtrl(" ")).toBe("\x00");
    expect(withCtrl("1")).toBe("1");
    expect(withCtrl("\x1b[A")).toBe("\x1b[A");
  });
});
