import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { ReactNode } from "react";
import { describe, expect, it } from "vitest";
import { Dialog } from "./ui/dialog";
import { AgentsLayer } from "./ui/layer";
import { Menu, MenuItem, MenuPopup, MenuTrigger } from "./ui/menu";
import { Popover, PopoverPopup, PopoverTrigger } from "./ui/popover";
import { Select } from "./ui/select";

/** The portaled layers the agents surface opens: a dialog, a popover, a menu and a select. */
function Layers() {
  return (
    <>
      <Dialog open onClose={() => {}} title="A dialog">
        <p>dialog body</p>
      </Dialog>
      <Popover open>
        <PopoverTrigger>pop</PopoverTrigger>
        <PopoverPopup>popover body</PopoverPopup>
      </Popover>
      <Menu open>
        <MenuTrigger>menu</MenuTrigger>
        <MenuPopup>
          <MenuItem>menu item</MenuItem>
        </MenuPopup>
      </Menu>
    </>
  );
}

const sizes = [
  { value: "a", label: "Small" },
  { value: "b", label: "Large" },
];

const layerOf = (text: string) => screen.getAllByText(text).at(-1)!.closest("[data-agents]");

/** A select on its own (the open dialog makes the rest of the page inert). */
async function openSelect(wrapper?: (p: { children: ReactNode }) => ReactNode) {
  render(<Select label="Size" value="a" options={sizes} onChange={() => {}} />, { wrapper });
  await userEvent.click(screen.getByRole("combobox", { name: "Size" }));
  return within(await screen.findByRole("listbox")).getByText("Large");
}

describe("popups opened from the agents surface", () => {
  const surface = ({ children }: { children: ReactNode }) => (
    <div data-agents>
      <AgentsLayer>{children}</AgentsLayer>
    </div>
  );

  it("portal out of it to the body but carry its tokens (the portal is a layer, not a fill)", async () => {
    render(<Layers />, { wrapper: surface });
    for (const text of ["dialog body", "popover body", "menu item"]) {
      const layer = layerOf(text);
      expect(layer, text).not.toBeNull();
      // Not the surface's own element: they're portaled to <body>.
      expect(layer!.hasAttribute("data-layer"), text).toBe(true);
    }
  });

  it("a select's list too", async () => {
    expect((await openSelect(surface)).closest("[data-agents][data-layer]")).not.toBeNull();
  });

  it("keep the dashboard's look anywhere else (the share page, Connect)", async () => {
    render(<Layers />);
    for (const text of ["dialog body", "popover body", "menu item"]) expect(layerOf(text), text).toBeNull();
  });

  it("a select's list too, outside", async () => {
    expect((await openSelect()).closest("[data-agents]")).toBeNull();
  });
});
