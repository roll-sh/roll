"use client";

/** Ported from T3 Code's `components/ui/popover.tsx` (MIT), in roll's colours. */

import { Popover as PopoverPrimitive } from "@base-ui/react/popover";
import { cn, mergeClass, glass, popupMotion } from "./cn";
import { useLayer } from "./layer";

export const Popover = PopoverPrimitive.Root;
export const PopoverTrigger = PopoverPrimitive.Trigger;
export const PopoverClose = PopoverPrimitive.Close;
export const PopoverTitle = PopoverPrimitive.Title;

export function PopoverPopup({
  children,
  className,
  side = "bottom",
  align = "center",
  sideOffset = 4,
  alignOffset = 0,
  anchor,
  ...props
}: PopoverPrimitive.Popup.Props & {
  side?: PopoverPrimitive.Positioner.Props["side"];
  align?: PopoverPrimitive.Positioner.Props["align"];
  sideOffset?: PopoverPrimitive.Positioner.Props["sideOffset"];
  alignOffset?: PopoverPrimitive.Positioner.Props["alignOffset"];
  anchor?: PopoverPrimitive.Positioner.Props["anchor"];
}) {
  const layer = useLayer();
  return (
    <PopoverPrimitive.Portal {...layer}>
      <PopoverPrimitive.Positioner className="z-[130]" side={side} align={align} sideOffset={sideOffset} alignOffset={alignOffset} anchor={anchor} collisionPadding={8}>
        <PopoverPrimitive.Popup
          className={mergeClass(cn("relative flex max-h-(--available-height) max-w-(--available-width) rounded-lg text-fg outline-none [-webkit-app-region:no-drag]", glass, popupMotion), className)}
          {...props}
        >
          {children}
        </PopoverPrimitive.Popup>
      </PopoverPrimitive.Positioner>
    </PopoverPrimitive.Portal>
  );
}
