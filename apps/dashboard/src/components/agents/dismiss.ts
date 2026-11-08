import { useEffect, useRef, type RefObject } from "react";

/**
 * Closes a small hand-built popup (a field's suggestions, a picker) the way Base UI's dismiss does:
 * a press outside every one of `inside` (capture phase, so a handler that stops it can't keep it
 * open), or focus moving somewhere outside them. Never on a blur alone: Safari doesn't focus a
 * clicked button, so a blur says nothing about where the press went (docs/code/agents-design.md,
 * the user's browser); the 120 ms timeouts this replaces closed a picker before its click landed.
 */
export function useDismiss(open: boolean, inside: readonly RefObject<HTMLElement | null>[], close: () => void) {
  const latest = useRef({ inside, close });
  latest.current = { inside, close };
  useEffect(() => {
    if (!open) return;
    const outside = (t: EventTarget | null) => t instanceof Node && !latest.current.inside.some((r) => r.current?.contains(t));
    const onPress = (e: PointerEvent) => outside(e.target) && latest.current.close();
    const onFocus = (e: FocusEvent) => outside(e.target) && latest.current.close();
    document.addEventListener("pointerdown", onPress, true);
    document.addEventListener("focusin", onFocus);
    return () => {
      document.removeEventListener("pointerdown", onPress, true);
      document.removeEventListener("focusin", onFocus);
    };
  }, [open]);
}
