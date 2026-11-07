"use client";

/** Ported from T3 Code's `components/ui/switch.tsx` (MIT): on is roll's accent; the thumb slides, and squashes a little while pressed. */

import { Switch as SwitchPrimitive } from "@base-ui/react/switch";
import { cn, mergeClass } from "./cn";

export function Switch({ className, size = "default", ...props }: SwitchPrimitive.Root.Props & { size?: "default" | "sm" }) {
  return (
    <SwitchPrimitive.Root
      className={mergeClass(
        cn(
          "inline-flex h-[calc(var(--thumb-size)+2px)] w-[calc(var(--thumb-size)*2-2px)] shrink-0 cursor-pointer items-center rounded-full p-[2px] outline-none transition-[background-color,box-shadow] duration-200 focus-visible:ring-2 focus-visible:ring-accent/60 data-checked:bg-accent data-unchecked:bg-line-2 data-disabled:cursor-not-allowed data-disabled:opacity-60 motion-reduce:transition-none",
          size === "sm" ? "[--thumb-size:--spacing(3.5)]" : "[--thumb-size:--spacing(4)]",
        ),
        className,
      )}
      {...props}
    >
      <SwitchPrimitive.Thumb className="pointer-events-none block size-[calc(var(--thumb-size)-2px)] shrink-0 origin-left rounded-(--thumb-size) bg-white shadow-sm [transition:translate_.15s,border-radius_.15s,scale_.1s_.1s,transform-origin_.15s] in-[[role=switch]:active]:scale-x-110 data-checked:origin-right data-checked:translate-x-[calc(var(--thumb-size)-4px)] motion-reduce:transition-none" />
    </SwitchPrimitive.Root>
  );
}
