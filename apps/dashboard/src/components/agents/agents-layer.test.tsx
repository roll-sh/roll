import { render, screen } from "@testing-library/react";
import type { ReactNode } from "react";
import { describe, expect, it } from "vitest";
import { Dialog } from "./ui/dialog";
import { AgentsLayer } from "./ui/layer";
import { Menu, MenuItem, MenuPopup, MenuTrigger } from "./ui/menu";
import { Popover, PopoverPopup, PopoverTrigger } from "./ui/popover";

/** Portaled layers the agents surface opens: a dialog, a popover and a menu. */
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

const layerOf = (text: string) => screen.getAllByText(text).at(-1)!.closest("[data-agents]");

describe("popups opened from the agents surface", () => {
  it("portal to the body but carry its tokens (the portal is a layer, not a fill)", () => {
    render(<Layers />, {
      wrapper: ({ children }: { children: ReactNode }) => (
        <div data-agents>
          <AgentsLayer>{children}</AgentsLayer>
        </div>
      ),
    });
    for (const text of ["dialog body", "popover body", "menu item"]) expect(layerOf(text)?.hasAttribute("data-layer"), text).toBe(true);
  });

  it("keep the dashboard's look anywhere else (the share page, Connect)", () => {
    render(<Layers />);
    for (const text of ["dialog body", "popover body", "menu item"]) expect(layerOf(text), text).toBeNull();
  });
});
