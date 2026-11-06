"use client";

/** Ported from T3 Code's `components/ui/menu.tsx` (MIT), in roll's colours. */

import { Menu as MenuPrimitive } from "@base-ui/react/menu";
import { Check, ChevronRight } from "lucide-react";
import type { ComponentProps } from "react";
import { cn, mergeClass, glass, itemRow, popupMotion } from "./cn";
import { useLayer } from "./layer";

export const Menu = MenuPrimitive.Root;
export const MenuTrigger = MenuPrimitive.Trigger;
export const MenuGroup = MenuPrimitive.Group;
export const MenuRadioGroup = MenuPrimitive.RadioGroup;
export const MenuSub = MenuPrimitive.SubmenuRoot;

export function MenuPopup({
  children,
  className,
  side = "bottom",
  align = "start",
  sideOffset = 4,
  alignOffset,
  anchor,
  ...props
}: MenuPrimitive.Popup.Props & {
  side?: MenuPrimitive.Positioner.Props["side"];
  align?: MenuPrimitive.Positioner.Props["align"];
  sideOffset?: MenuPrimitive.Positioner.Props["sideOffset"];
  alignOffset?: MenuPrimitive.Positioner.Props["alignOffset"];
  anchor?: MenuPrimitive.Positioner.Props["anchor"];
}) {
  const layer = useLayer();
  return (
    <MenuPrimitive.Portal {...layer}>
      <MenuPrimitive.Positioner className="z-[130]" side={side} align={align} sideOffset={sideOffset} alignOffset={alignOffset} anchor={anchor}>
        <MenuPrimitive.Popup
          className={mergeClass(cn("relative flex min-w-[min(10rem,calc(100vw-2rem))] max-w-[calc(100vw-2rem)] rounded-lg outline-none [-webkit-app-region:no-drag]", glass, popupMotion), className)}
          {...props}
        >
          <div className="max-h-(--available-height) w-full overflow-y-auto p-1">{children}</div>
        </MenuPrimitive.Popup>
      </MenuPrimitive.Positioner>
    </MenuPrimitive.Portal>
  );
}

export function MenuItem({ className, variant = "default", ...props }: MenuPrimitive.Item.Props & { variant?: "default" | "destructive" }) {
  return (
    <MenuPrimitive.Item
      className={mergeClass(cn(itemRow, variant === "destructive" && "text-danger data-highlighted:bg-danger/10 [&>svg]:text-current!"), className)}
      data-variant={variant}
      {...props}
    />
  );
}

export function MenuCheckboxItem({ className, children, ...props }: MenuPrimitive.CheckboxItem.Props) {
  return (
    <MenuPrimitive.CheckboxItem className={mergeClass(cn(itemRow, "grid grid-cols-[minmax(0,1fr)_1rem] pe-2"), className)} {...props}>
      <span className="min-w-0 truncate">{children}</span>
      <MenuPrimitive.CheckboxItemIndicator className="col-start-2 text-fg">
        <Check className="size-3.5" />
      </MenuPrimitive.CheckboxItemIndicator>
    </MenuPrimitive.CheckboxItem>
  );
}

/** A radio row: the picked one has a soft fill and a check (T3's `MenuRadioItem` with its indicator). */
export function MenuRadioItem({ className, children, ...props }: MenuPrimitive.RadioItem.Props) {
  return (
    <MenuPrimitive.RadioItem className={mergeClass(cn(itemRow, "data-checked:bg-fg/[0.06]"), className)} {...props}>
      <span className="min-w-0 flex-1">{children}</span>
      <MenuPrimitive.RadioItemIndicator aria-hidden className="flex shrink-0 text-fg">
        <Check className="size-3.5" />
      </MenuPrimitive.RadioItemIndicator>
    </MenuPrimitive.RadioItem>
  );
}

export function MenuGroupLabel({ className, ...props }: MenuPrimitive.GroupLabel.Props) {
  return <MenuPrimitive.GroupLabel className={mergeClass(cn("px-2 py-1.5 text-[11.5px] font-medium text-subtle"), className)} {...props} />;
}

export function MenuSeparator({ className, ...props }: MenuPrimitive.Separator.Props) {
  return <MenuPrimitive.Separator className={mergeClass(cn("mx-2 my-1 h-px bg-line"), className)} {...props} />;
}

export function MenuShortcut({ className, ...props }: ComponentProps<"kbd">) {
  // A key a touch screen has no keyboard for: not on a phone.
  return <kbd className={cn("ms-auto font-sans text-[11px] tracking-widest text-subtle pointer-coarse:hidden", className)} {...props} />;
}

export function MenuSubTrigger({ className, children, ...props }: MenuPrimitive.SubmenuTrigger.Props) {
  return (
    <MenuPrimitive.SubmenuTrigger className={mergeClass(cn(itemRow, "data-popup-open:bg-fg/[0.08]"), className)} {...props}>
      {children}
      <ChevronRight className="ms-auto -me-0.5 opacity-80" />
    </MenuPrimitive.SubmenuTrigger>
  );
}

export function MenuSubPopup({ align = "start", alignOffset = -5, sideOffset = 0, ...props }: ComponentProps<typeof MenuPopup>) {
  return <MenuPopup side="inline-end" align={align} alignOffset={alignOffset} sideOffset={sideOffset} {...props} />;
}
