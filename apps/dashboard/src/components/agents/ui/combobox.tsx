"use client";

/** Ported from T3 Code's `components/ui/combobox.tsx` (MIT), in roll's colours: a searchable list. */

import { Combobox as ComboboxPrimitive } from "@base-ui/react/combobox";
import { Search } from "lucide-react";
import { cn, mergeClass } from "./cn";

export const Combobox = ComboboxPrimitive.Root;

/** The search row at the top of a list: an icon and the field over a hairline that runs edge to edge (T3's `ComboboxSearchInput`). */
export function ComboboxSearchInput({ className, ...props }: ComboboxPrimitive.Input.Props) {
  return (
    <div className="relative min-w-0 shrink-0 border-b border-line px-3 py-1.5">
      <Search aria-hidden className="pointer-events-none absolute top-1/2 left-3 size-3.5 -translate-y-1/2 text-subtle" />
      <ComboboxPrimitive.Input className={mergeClass(cn("h-7 w-full bg-transparent ps-5 text-[13px] leading-7 text-fg outline-none placeholder:text-subtle"), className)} {...props} />
    </div>
  );
}

export function ComboboxList({ className, ...props }: ComboboxPrimitive.List.Props) {
  return <ComboboxPrimitive.List className={mergeClass(cn("min-h-0 flex-1 overflow-y-auto overscroll-contain not-empty:scroll-py-1 not-empty:p-1"), className)} {...props} />;
}

export function ComboboxItem({ className, children, ...props }: ComboboxPrimitive.Item.Props) {
  return (
    <ComboboxPrimitive.Item
      className={mergeClass(
        cn(
          "flex min-h-7 cursor-pointer items-center gap-2 rounded-sm px-2 py-1 text-[13px] text-fg outline-none not-data-disabled:hover:bg-fg/[0.08] data-disabled:pointer-events-none data-disabled:opacity-60 data-highlighted:bg-fg/[0.08] [&_svg]:pointer-events-none [&_svg]:shrink-0",
        ),
        className,
      )}
      {...props}
    >
      {children}
    </ComboboxPrimitive.Item>
  );
}

export function ComboboxEmpty({ className, ...props }: ComboboxPrimitive.Empty.Props) {
  return <ComboboxPrimitive.Empty className={mergeClass(cn("px-2 py-6 text-center text-[13px] text-subtle empty:hidden"), className)} {...props} />;
}
