"use client";

/**
 * Ported from T3 Code's `components/ui/dialog.tsx` and `dialog-styles.ts` (MIT), in roll's
 * tokens (docs/code/agents-design.md, "Dialogs and the palette"): the scrim, then a centred
 * sheet that grows out of what opened it (scale 0.98 and a fade, 200 ms; T3's dialog grows from
 * its centre, roll's from the click or the focused control) and goes the same way. Escape and a
 * press outside close it; focus is trapped while it's open and goes back after. Reduced motion:
 * no transition.
 *
 * The Root has to stay mounted for the way out to play: `{open && <Dialog open …/>}` unmounts it
 * at once. A dialog its caller shows for a value (`{profile && …}`) takes `useKept`, which keeps the
 * last value until the dialog says it's gone (`onExited`).
 */

import { Dialog as Primitive } from "@base-ui/react/dialog";
import { X } from "lucide-react";
import { useCallback, useLayoutEffect, useRef, useState, type CSSProperties, type ReactNode } from "react";
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

/**
 * Keeps what a dialog shows while it plays its way out. `value` is what the caller would have
 * mounted it for (`null`, `undefined` or `false`: closed). Mount the dialog while `value` (the kept
 * one) is set, with `open` and `onExited`; `key` changes for each fresh opening, so its state starts
 * over (opened again while it's still leaving, it turns around as it is).
 */
export function useKept<T>(value: T | null | undefined | false): { open: boolean; value: T | null; key: number; onExited: () => void } {
  const open = value !== null && value !== undefined && value !== false;
  const [kept, setKept] = useState<{ value: T | null; key: number; open: boolean }>(() => ({ value: open ? value : null, key: 0, open }));
  if (open && (kept.value !== value || !kept.open)) setKept({ value, key: kept.value === null ? kept.key + 1 : kept.key, open: true });
  else if (!open && kept.open) setKept({ ...kept, open: false });
  const onExited = useCallback(() => setKept((k) => (k.open ? k : { ...k, value: null })), []);
  return { open, value: kept.value, key: kept.key, onExited };
}

/**
 * The dialog's frame without its layout: the scrim, a popup that grows from what opened it and goes
 * back the same way, focus trapped and given back. `Dialog` is this with a title and a footer; a
 * surface with its own layout (the launcher) uses it directly.
 */
export function DialogSheet({
  open,
  onClose,
  onExited,
  busy = false,
  label,
  container,
  className,
  style,
  backdropClassName,
  viewportClassName,
  initialFocus,
  children,
}: {
  open: boolean;
  onClose: () => void;
  /** It's gone: its way out has finished (`useKept`'s `onExited`). */
  onExited?: () => void;
  /** Working on something that mustn't be left half done: Escape, a press outside and × wait. */
  busy?: boolean;
  /** Its accessible name when it has no `Title`. */
  label?: string;
  /** Portaled here instead of the body (the launcher sits inside the agents surface). */
  container?: HTMLElement | null;
  className?: string;
  style?: CSSProperties;
  /** The scrim's place and look (replaces the default: `cn` doesn't merge conflicting classes). */
  backdropClassName?: string;
  /** Where the popup sits; the default centres it a third of the way down. */
  viewportClassName?: string;
  /** What takes focus when it opens; the default keeps a field that asked for it, else the sheet. */
  initialFocus?: () => HTMLElement | null;
  children: ReactNode;
}) {
  const origin = useRef<{ x: number; y: number } | null>(null);
  // Base UI plays the way in only when `open` turns on in a Root that's already mounted: one that
  // mounts open (every dialog its caller mounts to show) appeared at once, with no starting style.
  // So the Root's first commit is closed, and it opens before the browser paints.
  const [mounted, setMounted] = useState(false);
  useLayoutEffect(() => setMounted(true), []);
  const shown = open && mounted;
  if (shown && !origin.current && typeof window !== "undefined") origin.current = openedFrom();
  if (!shown) origin.current = null;
  const popup = useRef<HTMLDivElement | null>(null);
  // A callback ref: Base UI mounts the popup a commit after `open` turns on, so an effect on `open`
  // found no element and the origin never applied (it grew from its centre).
  const attach = useCallback((el: HTMLDivElement | null) => {
    popup.current = el;
    const o = origin.current;
    // A caller's own origin (`style`, set before this runs) wins.
    if (!el || !o || el.style.transformOrigin) return;
    const r = el.getBoundingClientRect();
    el.style.transformOrigin = `${Math.round(o.x - r.left)}px ${Math.round(o.y - r.top)}px`;
  }, []);
  return (
    <Primitive.Root open={shown} onOpenChange={(next) => !next && !busy && onClose()} onOpenChangeComplete={(now) => !now && onExited?.()}>
      <Primitive.Portal container={container ?? undefined}>
        <Primitive.Backdrop className={cn(backdropClassName ?? "fixed inset-0 z-[125] bg-(--scrim) backdrop-blur-[3px]", "transition-opacity duration-200 ease-out data-ending-style:opacity-0 data-ending-style:duration-[170ms] data-starting-style:opacity-0 motion-reduce:transition-none")} />
        <Primitive.Viewport className={viewportClassName ?? "fixed inset-0 z-[125] grid grid-rows-[1fr_auto_3fr] justify-items-center overflow-y-auto p-3 sm:p-4"}>
          <Primitive.Popup
            ref={attach}
            aria-label={label}
            initialFocus={
              initialFocus ??
              (() => {
                const el = popup.current;
                const active = document.activeElement;
                return el && active instanceof HTMLElement && el.contains(active) && active !== el ? active : el;
              })
            }
            // In on the drawer curve over 200 ms, out on an ease-in over 170 ms (`POP_IN`/`POP_OUT`).
            className={cn(
              "outline-none transition-[scale,opacity] duration-200 ease-(--ease-drawer) data-ending-style:scale-98 data-ending-style:opacity-0 data-ending-style:duration-[170ms] data-ending-style:ease-in data-starting-style:scale-98 data-starting-style:opacity-0 motion-reduce:transition-none [-webkit-app-region:no-drag]",
              className,
            )}
            style={style}
          >
            {children}
          </Primitive.Popup>
        </Primitive.Viewport>
      </Primitive.Portal>
    </Primitive.Root>
  );
}

export function Dialog({
  open,
  onClose,
  onExited,
  title,
  lead,
  children,
  footer,
  className,
  busy = false,
}: {
  open: boolean;
  onClose: () => void;
  /** It's gone: its way out has finished (`useKept`'s `onExited`). */
  onExited?: () => void;
  title: ReactNode;
  lead?: ReactNode;
  children: ReactNode;
  /** Buttons, right-aligned under a hairline. */
  footer?: ReactNode;
  className?: string;
  /** Working on something that mustn't be left half done: Escape, a press outside and × wait. */
  busy?: boolean;
}) {
  return (
    <DialogSheet
      open={open}
      onClose={onClose}
      onExited={onExited}
      busy={busy}
      className={cn("row-start-2 flex max-h-[calc(100dvh-1.5rem)] w-[min(520px,100%)] min-w-0 flex-col rounded-2xl border border-line-2 bg-sheet text-fg shadow-sheet inset-shadow-highlight", className)}
    >
      <div className="flex items-start gap-2 px-5 pt-5 pb-3">
        <div className="min-w-0 flex-1 space-y-1">
          <Primitive.Title className="text-[17px] font-[590] tracking-[-0.025em]">{title}</Primitive.Title>
          {lead && <Primitive.Description className="text-[12.5px] text-muted">{lead}</Primitive.Description>}
        </div>
        <Primitive.Close aria-label="Close" disabled={busy} className="disabled:opacity-40 -mt-1 -mr-1 grid size-8 shrink-0 place-items-center rounded-lg text-subtle transition-colors duration-150 hover:bg-fg/[0.06] hover:text-fg">
          <X className="size-4" strokeWidth={1.75} />
        </Primitive.Close>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-5 pb-5">{children}</div>
      {footer && <div className="flex items-center justify-end gap-2 border-t border-line px-5 py-3">{footer}</div>}
    </DialogSheet>
  );
}

/** A dialog's title, for a `DialogSheet` with its own layout. */
export const DialogTitle = Primitive.Title;
