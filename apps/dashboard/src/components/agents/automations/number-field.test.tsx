import { fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it, vi } from "vitest";
import { NumberField } from "./shared";

function Field({ initial, onValue, ...props }: { initial: number | null; onValue: (n: number | null) => void; min?: number; max?: number; optional?: boolean }) {
  const [value, setValue] = useState(initial);
  return (
    <NumberField
      aria-label="Tries"
      value={value}
      onChange={(n) => {
        setValue(n);
        onValue(n);
      }}
      {...props}
    />
  );
}

describe("NumberField", () => {
  it("lets a number be cleared and typed again without snapping to the minimum", async () => {
    const onValue = vi.fn();
    render(<Field initial={3} onValue={onValue} min={1} max={10} />);
    const field = screen.getByRole("spinbutton", { name: "Tries" });
    await userEvent.clear(field);
    expect(field).toHaveValue(null);
    expect(onValue).not.toHaveBeenCalled();
    await userEvent.type(field, "5");
    expect(field).toHaveValue(5);
    expect(onValue).toHaveBeenLastCalledWith(5);
  });

  it("marks a number out of range and keeps the last good one", async () => {
    const onValue = vi.fn();
    render(<Field initial={3} onValue={onValue} min={1} max={10} />);
    const field = screen.getByRole("spinbutton", { name: "Tries" });
    await userEvent.clear(field);
    await userEvent.type(field, "12");
    expect(onValue).toHaveBeenLastCalledWith(1);
    expect(field).toHaveAttribute("aria-invalid", "true");
    fireEvent.blur(field);
    expect(field).toHaveValue(1);
    expect(field).not.toHaveAttribute("aria-invalid");
  });

});
