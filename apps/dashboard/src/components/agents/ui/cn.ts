/** Join class names, skipping falsy ones (T3's `cn` without tailwind-merge: put overrides last and don't fight a base class). */
export function cn(...parts: (string | false | null | undefined)[]): string {
  return parts.filter(Boolean).join(" ");
}

/** `base` then the caller's class, which Base UI lets be a function of the part's state. */
export function mergeClass<S>(base: string, extra: string | ((state: S) => string | undefined) | undefined): string | ((state: S) => string) {
  if (typeof extra === "function") return (state: S) => cn(base, extra(state));
  return cn(base, extra);
}

/**
 * The floating surface every popup shares (T3's `dropdown-glass` and popup shadow) in roll's
 * tokens (docs/code/agents-design.md): the popover layer a little see-through over a blur, the
 * outline hairline, the top highlight and the pop shadow.
 */
export const glass = "border border-outline glass shadow-pop inset-shadow-highlight";

/** How popups grow out of their trigger and go (T3: scale 0.98 and a fade, about 150 ms). */
export const popupMotion =
  "origin-(--transform-origin) transition-[scale,opacity] duration-150 ease-out motion-safe:data-starting-style:scale-98 motion-safe:data-starting-style:opacity-0 data-ending-style:scale-98 data-ending-style:opacity-0 motion-reduce:transition-none";

/** A row in a menu, select or list (T3's item: 28px, a neutral fill when highlighted). */
export const itemRow =
  "flex min-h-7 cursor-pointer select-none items-center gap-2 rounded-sm px-2 py-1 text-[13px] text-fg outline-none data-highlighted:bg-fg/[0.08] data-disabled:pointer-events-none data-disabled:cursor-not-allowed data-disabled:opacity-60 [&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-3.5 [&>svg:not([class*='text-'])]:text-muted";
