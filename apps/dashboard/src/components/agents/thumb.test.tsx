import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { Segmented } from "./ui";

// jsdom lays nothing out: each toggle is 80px wide, side by side, in a group at the origin.
const original = HTMLElement.prototype.getBoundingClientRect;
beforeEach(() => {
  HTMLElement.prototype.getBoundingClientRect = function (this: HTMLElement) {
    if (this.getAttribute("role") === "group") return DOMRect.fromRect({ x: 0, y: 0, width: 240, height: 28 });
    const i = this.parentElement ? [...this.parentElement.querySelectorAll(":scope > button")].indexOf(this as HTMLButtonElement) : -1;
    return i >= 0 ? DOMRect.fromRect({ x: 2 + i * 80, y: 2, width: 80, height: 24 }) : DOMRect.fromRect({ x: 0, y: 0, width: 0, height: 0 });
  };
});
afterEach(() => {
  HTMLElement.prototype.getBoundingClientRect = original;
});

function Picker() {
  const [v, setV] = useState<"date" | "duration">("date");
  return (
    <Segmented
      label="Snooze by"
      value={v}
      onChange={setV}
      options={[
        { value: "date", label: "Date and time" },
        { value: "duration", label: "Duration" },
      ]}
    />
  );
}

describe("a segmented control's sliding thumb", () => {
  it("is one element under the choices that moves to the one picked; the choices only change colour", async () => {
    render(<Picker />);
    const group = screen.getByRole("group", { name: "Snooze by" });
    const thumb = group.querySelector<HTMLElement>("[data-thumb]")!;
    expect(thumb).toHaveAttribute("aria-hidden", "true");
    expect(thumb.style.transform).toBe("translate(2px, 2px)");
    expect(thumb.style.width).toBe("80px");
    // The fill is the thumb's, not the pressed button's.
    expect(screen.getByRole("button", { name: "Date and time" }).className).not.toMatch(/data-pressed:bg-/);
    await userEvent.click(screen.getByRole("button", { name: "Duration" }));
    expect(screen.getByRole("button", { name: "Duration" })).toHaveAttribute("aria-pressed", "true");
    // The same thumb, moved over (a CSS transition plays it).
    expect(group.querySelector("[data-thumb]")).toBe(thumb);
    expect(thumb.style.transform).toBe("translate(82px, 2px)");
  });
});
