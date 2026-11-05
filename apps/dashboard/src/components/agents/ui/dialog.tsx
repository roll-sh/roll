"use client";

/**
 * Ported from T3 Code's `components/ui/dialog.tsx` and `dialog-styles.ts` (MIT), in roll's
 * tokens (docs/code/agents-design.md, "Dialogs and the palette"): the scrim, then a centred
 * sheet that grows out of what opened it (scale 0.98 and a fade, 200 ms; T3's dialog grows from
 * its centre, roll's from the click or the focused control) and goes the same way. Escape and a
 * press outside close it; focus is trapped while it's open and goes back after. Reduced motion:
 * no transition.
 */

import { Dialog as Primitive } from "@base-ui/react/dialog";
import { X } from "lucide-react";
import { useLayoutEffect, useRef, type ReactNode } from "react";
import { cn } from "./cn";

// Where the last press was: a dialog opened by it grows from there (Safari doesn't focus a
// clicked button, so the focused element can't say).
let press: { x: number; y: number; at: number } | null = null;
if (typeof window !== "undefined") {
  window.addEventListener("pointerdown", (e) => (press = { x: e.clientX, y: e.clientY, at: performance.now() }), { capture: true, passive: true });
}

/** Where a dialog opening now grows from: the press that opened it, else the focused control, else its centre. */
function openedFrom(): { x: number; y: number } | null {
  if (press && performance.now() - press.at < 1500) return press;
  const el = document.activeElement;
  if (el && el !== document.body) {
    const r = el.getBoundingClientRect();
    if (r.width || r.height) return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  }
  return null;
}

export function Dialog({
  open,
  onClose,
  label,
  title,
  lead,
  children,
  footer,
  className,
}: {
  open: boolean;
  onClose: () => void;
  /** Its accessible name, when it differs from `title`. */
  label?: string;
  title: ReactNode;
  lead?: ReactNode;
  children: ReactNode;
  /** Buttons, right-aligned under a hairline. */
  footer?: ReactNode;
  className?: string;
}) {
  const popup = useRef<HTMLDivElement>(null);
  const origin = useRef<{ x: number; y: number } | null>(null);
  if (open && !origin.current && typeof window !== "undefined") origin.current = openedFrom();
  if (!open) origin.current = null;
  useLayoutEffect(() => {
    const el = popup.current;
    const o = origin.current;
    if (!open || !el || !o) return;
    const r = el.getBoundingClientRect();
    el.style.transformOrigin = `${Math.round(o.x - r.left)}px ${Math.round(o.y - r.top)}px`;
  }, [open]);
  return (
    <Primitive.Root open={open} onOpenChange={(next) => !next && onClose()}>
      <Primitive.Portal>
        <Primitive.Backdrop className="fixed inset-0 z-[125] bg-(--scrim) backdrop-blur-[3px] transition-opacity duration-200 ease-out data-ending-style:opacity-0 data-starting-style:opacity-0 motion-reduce:transition-none" />
        <Primitive.Viewport className="fixed inset-0 z-[125] grid grid-rows-[1fr_auto_3fr] justify-items-center overflow-y-auto p-3 sm:p-4">
          <Primitive.Popup
            ref={popup}
            aria-label={label}
            className={cn(
              "row-start-2 flex max-h-[calc(100dvh-1.5rem)] w-[min(520px,100%)] min-w-0 flex-col rounded-2xl border border-line-2 bg-sheet text-fg shadow-sheet inset-shadow-highlight outline-none transition-[scale,opacity] duration-200 ease-out data-ending-style:scale-98 data-ending-style:opacity-0 data-starting-style:scale-98 data-starting-style:opacity-0 motion-reduce:transition-none [-webkit-app-region:no-drag]",
              className,
            )}
          >
            <div className="flex items-start gap-2 px-5 pt-5 pb-3">
              <div className="min-w-0 flex-1 space-y-1">
                <Primitive.Title className="text-[17px] font-[590] tracking-[-0.025em]">{title}</Primitive.Title>
                {lead && <Primitive.Description className="text-[12.5px] text-muted">{lead}</Primitive.Description>}
              </div>
              <Primitive.Close aria-label="Close" className="-mt-1 -mr-1 grid size-8 shrink-0 place-items-center rounded-lg text-subtle transition-colors duration-150 hover:bg-fg/[0.06] hover:text-fg">
                <X className="size-4" strokeWidth={1.75} />
              </Primitive.Close>
            </div>
            <div className="min-h-0 flex-1 overflow-y-auto px-5 pb-5">{children}</div>
            {footer && <div className="flex items-center justify-end gap-2 border-t border-line px-5 py-3">{footer}</div>}
          </Primitive.Popup>
        </Primitive.Viewport>
      </Primitive.Portal>
    </Primitive.Root>
  );
}
