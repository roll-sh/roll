import { act, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useRef, useState } from "react";
import { describe, expect, it } from "vitest";
import { useFocusRescue } from "./focus-rescue";

/** A list whose rows go away when their action is used (Settle moves a row to its shelf). */
function Rows({ start }: { start: string[] }) {
  const root = useRef<HTMLDivElement>(null);
  useFocusRescue(root);
  const [rows, setRows] = useState(start);
  const [bar, setBar] = useState(true);
  return (
    <div ref={root}>
      <ul>
        {rows.map((r) => (
          <li key={r}>
            <button type="button">{r}</button>
            <button type="button" onClick={() => setRows((xs) => xs.filter((x) => x !== r))}>
              Settle {r}
            </button>
          </li>
        ))}
      </ul>
      <form>
        <input aria-label="Name" />
        {bar && (
          <button type="button" onClick={() => setBar(false)}>
            Discard
          </button>
        )}
      </form>
    </div>
  );
}

describe("focus that loses what held it", () => {
  it("goes to what took the row's place after a key (the next row, else the one before), never the page", async () => {
    render(<Rows start={["a", "b", "c"]} />);
    screen.getByRole("button", { name: "Settle b" }).focus();
    await userEvent.keyboard("{Enter}");
    expect(screen.queryByRole("button", { name: "Settle b" })).toBeNull();
    await act(async () => {});
    expect(screen.getByRole("button", { name: "c" })).toHaveFocus();
    // The last row: the one before it.
    screen.getByRole("button", { name: "Settle c" }).focus();
    await userEvent.keyboard("{Enter}");
    await act(async () => {});
    expect(screen.getByRole("button", { name: "a" })).toHaveFocus();
    // Discard's bar goes: the field before it.
    screen.getByRole("button", { name: "Discard" }).focus();
    await userEvent.keyboard("{Enter}");
    await act(async () => {});
    expect(screen.getByRole("textbox", { name: "Name" })).toHaveFocus();
  });

  it("leaves focus alone after a click (Safari doesn't focus what's clicked)", async () => {
    render(<Rows start={["a", "b"]} />);
    await userEvent.click(screen.getByRole("button", { name: "Settle a" }));
    await act(async () => {});
    expect(document.activeElement).toBe(document.body);
  });
});
