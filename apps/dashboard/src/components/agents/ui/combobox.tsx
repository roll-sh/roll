"use client";

/** Ported from T3 Code's `components/ui/combobox.tsx` (MIT), in roll's colours: a searchable list. */

import { Combobox as ComboboxPrimitive } from "@base-ui/react/combobox";
import { Search } from "lucide-react";
import { cn, mergeClass } from "./cn";

export const Combobox = ComboboxPrimitive.Root;

/** The search row at the top of a list: an icon, an underline that lights on focus (T3's `ComboboxSearchInput`). */
export function ComboboxSearchInput({ className, ...props }: ComboboxPrimitive.Input.Props) {
  return (
    <div className="min-w-0 shrink-0 px-3 pt-2.5">
      <div className="relative -translate-y-px border-b border-line pb-1.5 transition-colors focus-within:border-subtle">
        <Search aria-hidden className="pointer-events-none absolute top-1.5 left-0 size-3.5 text-subtle" />
        <ComboboxPrimitive.Input className={mergeClass(cn("h-6.5 w-full bg-transparent ps-5 text-[13px] leading-6.5 text-fg outline-none placeholder:text-subtle"), className)} {...props} />
      </div>
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
