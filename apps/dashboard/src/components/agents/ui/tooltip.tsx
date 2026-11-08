/**
 * A small label over an icon button (T3's `components/ui/tooltip.tsx`, MIT, on Base UI, without its
 * scroll tracker): it shows after a short hover, at once on keyboard focus, and never on touch (a
 * tap there is the action). The button keeps its own `aria-label`; the tooltip only shows it.
 */

import { Tooltip as TooltipPrimitive } from "@base-ui/react/tooltip";
import type { ReactElement } from "react";
import { cn } from "./cn";
import { useLayer } from "./layer";

export function Tip({ label, children, side = "top" }: { label: string; children: ReactElement<Record<string, unknown>>; side?: TooltipPrimitive.Positioner.Props["side"] }) {
  const layer = useLayer();
  return (
    <TooltipPrimitive.Root>
      <TooltipPrimitive.Trigger delay={350} closeDelay={0} render={children} />
      <TooltipPrimitive.Portal {...layer}>
        <TooltipPrimitive.Positioner className="z-[140]" side={side} sideOffset={6}>
          <TooltipPrimitive.Popup
            className={cn(
              "pointer-events-none rounded-md border border-outline bg-popover px-2 py-1 text-[11.5px] leading-tight text-fg shadow-pop inset-shadow-highlight",
              "origin-(--transform-origin) transition-[opacity,scale] duration-150 ease-out data-ending-style:opacity-0 data-starting-style:scale-98 data-starting-style:opacity-0 motion-reduce:transition-none",
            )}
          >
            {label}
          </TooltipPrimitive.Popup>
        </TooltipPrimitive.Positioner>
      </TooltipPrimitive.Portal>
    </TooltipPrimitive.Root>
  );
}
