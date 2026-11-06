"use client";

import { useState, type ReactNode } from "react";
import { Menu as Root, MenuItem as Item, MenuPopup, MenuTrigger } from "./ui/menu";

export interface MenuItem {
  label: string;
  icon?: ReactNode;
  onSelect: () => void;
  disabled?: boolean;
  danger?: boolean;
  /** Ask once more before `onSelect` ("Delete" → "Delete thread?"). */
  confirm?: string;
}

/**
 * A small menu under its trigger, on the agents' Base UI menu (`ui/menu.tsx`): portalled, so a
 * scrolling strip (the views bar) or a stacked header never clips it or takes its taps; arrow
 * keys, Home/End and typing move through it; it closes on a pick, a click outside or Escape.
 */
export function Menu({
  trigger,
  label,
  items,
  align = "end",
  className = "",
}: {
  trigger: ReactNode;
  label: string;
  items: (MenuItem | null | false)[];
  align?: "start" | "end";
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const [armed, setArmed] = useState<string | null>(null);
  const shown = items.filter((i): i is MenuItem => !!i);
  return (
    <span className={`relative inline-flex ${className}`} data-nodrag>
      <Root
        open={open}
        onOpenChange={(o) => {
          setOpen(o);
          // Disarmed as it opens, not as it closes: "Delete thread?" turned back into "Delete" while the menu faded.
          if (o) setArmed(null);
        }}
      >
        <MenuTrigger
          aria-label={label}
          title={label}
          className={`grid size-7 place-items-center rounded-md text-subtle transition-colors duration-150 hover:bg-fg/[0.07] hover:text-fg ${open ? "bg-fg/[0.07] text-fg" : ""}`}
        >
          {trigger}
        </MenuTrigger>
        <MenuPopup aria-label={label} align={align} sideOffset={6} className="min-w-48">
          {shown.map((i) => {
            const asking = armed === i.label;
            return (
              <Item
                key={i.label}
                disabled={i.disabled}
                variant={i.danger ? "destructive" : "default"}
                closeOnClick={!i.confirm || asking}
                onClick={() => {
                  if (i.confirm && !asking) return setArmed(i.label);
                  i.onSelect();
                }}
              >
                <span className={`grid size-3.5 place-items-center [&>svg]:size-3.5 [&>svg]:stroke-[1.75] ${i.danger ? "" : "text-muted"}`}>{i.icon}</span>
                {asking ? i.confirm : i.label}
              </Item>
            );
          })}
        </MenuPopup>
      </Root>
    </span>
  );
}
