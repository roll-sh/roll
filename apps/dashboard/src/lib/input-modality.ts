/**
 * Whether the last thing the person did was on the keyboard (not a pointer). Focus that code moves
 * for keyboard users (back to a card when something closes, back to what had it before ⌘K) is
 * left alone after a click or a tap: Safari and Firefox on macOS don't focus what's clicked, and a
 * focus moved by script after a click can paint a focus ring nobody asked for.
 */
let keyboard = false;

if (typeof window !== "undefined") {
  window.addEventListener("keydown", () => (keyboard = true), true);
  window.addEventListener("pointerdown", () => (keyboard = false), true);
}

export function lastInputWasKeyboard(): boolean {
  return keyboard;
}
