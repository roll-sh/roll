"use client";

import { useEffect, useRef, useState, type ReactNode } from "react";

export interface MenuItem {
  label: string;
  icon?: ReactNode;
  onSelect: () => void;
  disabled?: boolean;
  danger?: boolean;
  /** Ask once more before `onSelect` ("Delete" → "Delete thread?"). */
  confirm?: string;
}

/** A small popover menu under its trigger: closes on a pick, a click outside or Escape. */
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
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => !box.current?.contains(e.target as Node) && setOpen(false);
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopPropagation();
      setOpen(false);
    };
    window.addEventListener("pointerdown", onDown, true);
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("keydown", onKey, true);
    };
  }, [open]);
  useEffect(() => {
    if (!open) setArmed(null);
  }, [open]);
  const shown = items.filter((i): i is MenuItem => !!i);
  return (
    <div ref={box} className={`relative ${className}`} data-nodrag>
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={label}
        title={label}
        className={`grid size-7 place-items-center rounded-md text-subtle transition-colors duration-150 hover:bg-fg/[0.07] hover:text-fg ${open ? "bg-fg/[0.07] text-fg" : ""}`}
      >
        {trigger}
      </button>
      {open && (
        <div
          role="menu"
          aria-label={label}
          className={`pop absolute top-full z-50 mt-1.5 min-w-48 rounded-[10px] border border-outline p-1 shadow-pop inset-shadow-highlight glass ${align === "end" ? "right-0" : "left-0"}`}
        >
          {shown.map((i) => {
            const asking = armed === i.label;
            return (
              <button
                key={i.label}
                type="button"
                role="menuitem"
                disabled={i.disabled}
                onClick={() => {
                  if (i.confirm && !asking) return setArmed(i.label);
                  setOpen(false);
                  i.onSelect();
                }}
                className={`flex h-7 w-full items-center gap-2 rounded-md px-2 text-left text-[13px] transition-colors duration-100 disabled:opacity-35 ${
                  i.danger ? "text-danger hover:bg-danger/10" : "text-fg hover:bg-fg/[0.07]"
                }`}
              >
                <span className={`grid size-3.5 place-items-center [&>svg]:size-3.5 [&>svg]:stroke-[1.75] ${i.danger ? "" : "text-muted"}`}>{i.icon}</span>
                {asking ? i.confirm : i.label}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
