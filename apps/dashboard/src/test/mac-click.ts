import { fireEvent } from "@testing-library/react";

/** What takes focus on a click in Safari and Firefox on macOS: text fields, and elements with a tabindex that aren't buttons or links. */
const TAKES_FOCUS = 'input:not([type=button]):not([type=checkbox]):not([type=radio]):not([type=submit]):not([type=reset]), textarea, select, [contenteditable="true"], [tabindex]:not(button):not(a):not([role=button]):not([role=menuitem]):not([role=option]):not([role=switch]):not([role=radio]):not([role=checkbox]):not([role=tab])';

/**
 * A click as Safari and Firefox on macOS make it (Chrome and Linux WebKit focus the button; these
 * don't): pointer and mouse events and the click, but a button, link, checkbox or menu row never
 * takes focus. Unless the mousedown is cancelled, what had focus blurs (focusout with no
 * `relatedTarget`) and `document.activeElement` is the body; a text field still takes focus.
 */
export function macClick(el: Element) {
  const opts = { bubbles: true, cancelable: true, button: 0, buttons: 1 };
  fireEvent.pointerDown(el, { ...opts, pointerType: "mouse", pointerId: 1, isPrimary: true });
  const down = fireEvent.mouseDown(el, opts);
  if (down) {
    const target = el.closest<HTMLElement>(TAKES_FOCUS);
    if (target) target.focus();
    else if (document.activeElement instanceof HTMLElement && document.activeElement !== document.body) document.activeElement.blur();
  }
  fireEvent.pointerUp(el, { ...opts, buttons: 0, pointerType: "mouse", pointerId: 1, isPrimary: true });
  fireEvent.mouseUp(el, { ...opts, buttons: 0 });
  fireEvent.click(el, { ...opts, buttons: 0 });
}
