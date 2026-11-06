import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";
import { macClick } from "@/test/mac-click";
import { TemplateInput } from "./automations/template-input";

const VARS = [
  { path: "run.id", group: "Run" },
  { path: "event.kind", group: "Event" },
];

function Field() {
  const [v, setV] = useState("");
  return (
    <>
      <TemplateInput label="Title" value={v} onChange={setV} variables={VARS} />
      <p>elsewhere</p>
    </>
  );
}
const variables = () => screen.queryByRole("listbox", { name: "Variables" });

describe("a field's variables (hand-built suggestions)", () => {
  it("a Safari click on a variable picks it: a blur alone doesn't close the list first", async () => {
    render(<Field />);
    const field = screen.getByRole("combobox", { name: "Title" });
    await userEvent.click(field);
    await userEvent.keyboard("{{{{");
    const list = await screen.findByRole("listbox", { name: "Variables" });
    // Safari: the field blurs with no relatedTarget before the click lands (a slow press: the old
    // 120 ms timeout had closed the list by then).
    fireEvent.blur(field, { relatedTarget: null });
    await act(() => new Promise((r) => setTimeout(r, 200)));
    expect(variables()).toBe(list);
    macClick(screen.getByRole("option", { name: /event\.kind/ }));
    expect(field).toHaveValue("{{ event.kind }}");
    await waitFor(() => expect(variables()).toBeNull());
  });

  it("closes on a press outside it", async () => {
    render(<Field />);
    await userEvent.click(screen.getByRole("combobox", { name: "Title" }));
    await userEvent.keyboard("{{{{");
    await screen.findByRole("listbox", { name: "Variables" });
    fireEvent.pointerDown(screen.getByText("elsewhere"));
    await waitFor(() => expect(variables()).toBeNull());
  });
});
